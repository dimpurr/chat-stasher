/**
 * The backfill leg's orchestration: enumerate → debt set → fetch bodies one by one
 * → hand off to the very same write-down exit the live leg uses.
 *
 * Three rules that must not be broken:
 *  1. persist immediately after clearing each debt ⇒ whenever it is killed, a
 *     restart carries on from the breakpoint;
 *  2. enumeration and body-fetching each use their own Pacer ⇒ the two segments are
 *     paced separately;
 *  2b. 🔴 W10 · **the two segments interleave within one tick** (one list page, then
 *     this tick's body budget) — a heavy account's first body must not wait for the
 *     whole list to be paged through. Sequential ordering is not a requirement here;
 *     "the newest is archived live, the old is filled in slowly, with a progress
 *     bar" is. See the `listPagesThisTick` note in runBackfill;
 *  3. any non-2xx / unrecognised shape / inability to persist ⇒ halt and write it
 *     into state.halted; never swallow it and keep spinning.
 *
 * 🔴 There is **no** path here that would really send a request by default: without
 *    an injected http port it is notWiredHttp, and calling that throws. Everything
 *    is tested against synthetic fixtures, with no logged-in session and none
 *    needed.
 */

import { getPlatformByOrigin, matchesResponseShape, type CapturedFetch } from '../contract';
import { dropDebt, enqueueDebts, nextDebt, settleDebt } from './debts';
import { recordFailure, type FailureEntry, type FailureReason } from './failures';
import {
  backfillPlanFor,
  canBackfillDetail,
  detailRequestInit,
  listRequestInit,
  unsupportedBackfillFor,
  DEFAULT_LIST_LIMIT,
  type BackfillRequestInit,
} from './enumerate';
import { formatProgress } from './progress';
import { DEFAULT_PACE, Pacer, drawDailyCap, systemClock, type BackfillPace, type Clock } from './pace';
import { systemRandom, type RandomFn } from './random';
import type { BackfillStore } from './store';
import {
  BACKFILL_STATE_VERSION,
  dayKeyOf,
  haltClassOf,
  initialState,
  isTransientReason,
  stateKey,
  transientRetryDelayMs,
  type BackfillState,
  type DetailOutcomeRecord,
  type EnumTruncation,
  type HaltReason,
  type HaltRecord,
  type StopReason,
} from './types';

export interface HttpResponse {
  status: number;
  text: string;
}

/**
 * 🔴 C23 · The channel can now express POST.
 *
 * Exactly one thing changed: an **optional** second parameter was added. Why this
 * rather than a new type:
 *  · every existing implementation (test fixtures, tabHttpPort) is `(url) => ...`,
 *    and in TypeScript writing fewer parameters is still assignable ⇒ **not one
 *    line has to change for them to keep working**;
 *  · on a GET segment the engine **still calls `http(url)` byte for byte**, not one
 *    argument more (see sendVia), so on ChatGPT's path not even the argument count
 *    changed.
 *
 * An omitted init ⇒ GET with no body. The method, the Content-Type and the body's
 * top-level keys are all closed sets, declared in lib/backfill/enumerate.ts and
 * enforced in lib/backfill/tab-port.ts.
 */
export type HttpPort = (url: string, init?: BackfillRequestInit) => Promise<HttpResponse>;

/** 🔴 GET with no body ⇒ fall back to the old call `http(url)`, byte for byte. This line is the back-compat landing point. */
async function sendVia(http: HttpPort, url: string, init: BackfillRequestInit): Promise<HttpResponse> {
  if (init.method === 'GET' && init.body === undefined) return http(url);
  return http(url, init);
}

/** The default port: it blows up on purpose. Not wired ⇒ never any network activity. */
export const notWiredHttp: HttpPort = async (url: string) => {
  throw new Error(`[chat-stasher] backfill http port is not wired (refused to fetch ${new URL(url).pathname})`);
};

/**
 * 🔴 C20 · The one thing the sink answers: **was it actually stored?**
 * Structurally compatible with entrypoints/background.ts's HandledResult (which is
 * returned as-is from there).
 */
export interface SinkOutcome {
  saved: boolean;
  /** The technical reason when it was not stored. Log and failure list only; never a conversation body. */
  reason?: string;
  /**
   * 🔴 The identity the write-down path **actually named it by**. The other half of
   * the root-cause fix: the debt key came from the list API's items[].id, the file
   * name came from "scrape the URL once more", and nothing checked the two against
   * each other in between. Reporting it back lets the engine reconcile on the spot.
   * An exit that does not report this field (undefined) ⇒ the check is skipped, and
   * the behaviour is byte-identical to C19.
   */
  sessionId?: string;
  /**
   * 🔴 W2 · **"This one was not delivered, but it must not be struck off."**
   *
   * This is a different thing from saved:false and has to be kept separate:
   *  · saved:false (without this field) = this item is **judged dead**: no retry,
   *    into the failure list, out of pending. It applies when "the conversation
   *    itself is the problem" (no safe file name can be produced, or the host
   *    explicitly nacks it as illegal).
   *  · retryLater:true = the exit is **temporarily** unreachable (this machine's
   *    host is not there). The conversation itself is fine, so: the debt stays in
   *    pending untouched, it does not go into the failure list, and this leg stops
   *    at once with a trace, resuming from the same item once the next heartbeat's
   *    `hello` succeeds.
   *
   * Without this field, "the host is not there" would be recorded as "this item is
   * done" — exactly the silent loss this project finds least acceptable (the debt
   * struck off, never retried, and nobody the wiser).
   */
  retryLater?: boolean;
}

/**
 * Turn the sink's return value into "stored / not stored".
 *
 * 🔴 **`undefined` counts as success**, and that compromise has to be written down:
 *    the old `sink: async (c) => { ... }` (returning void) is still legal by type,
 *    the engine hears no objection ⇒ it can only treat it as success.
 *    In other words: **an exit that reports no result can still clear a debt
 *    silently.**
 *    The production wiring (both call sites in entrypoints/background.ts) has been
 *    changed to return handleCaptured's result, so this hole does not exist on the
 *    production path; but if someone later wires up a sink that returns nothing,
 *    the hole reappears. Closing it completely would mean making the sink's return
 *    type mandatory — which would turn every existing test fixture red at once and
 *    is outside this change's scope, so it is **written down** here rather than
 *    quietly tightened.
 */
type SinkVerdict =
  | { ok: true }
  /** This item is judged dead (into the failure list, no retry, no longer queued). */
  | { ok: false; fatal: true; reason: FailureReason; detail: string }
  /** The exit is temporarily unreachable: the debt stays untouched and this leg stops. (W2) */
  | { ok: false; fatal: false; reason: string; detail: string };

function sinkVerdict(outcome: SinkOutcome | void | undefined, debtId: string): SinkVerdict {
  if (!outcome) return { ok: true };
  if (outcome.retryLater === true) {
    return {
      ok: false,
      fatal: false,
      reason: outcome.reason ?? 'host-unavailable',
      detail: 'the delivery destination is temporarily unreachable; the debt stays open',
    };
  }
  if (outcome.saved !== true) {
    return {
      ok: false, fatal: true, reason: 'not-saved',
      detail: outcome.reason ?? 'sink reported saved:false',
    };
  }
  if (outcome.sessionId !== undefined && outcome.sessionId !== debtId) {
    return {
      ok: false,
      fatal: true,
      reason: 'identity-mismatch',
      // Only the lengths, never the two ids themselves: a full conversation id goes into no log.
      detail: `debt key (len ${debtId.length}) != file identity (len ${outcome.sessionId.length})`,
    };
  }
  return { ok: true };
}

export interface BackfillOptions {
  platform: string;
  /** The platform origin, e.g. https://chatgpt.com. Must hit the platform table in lib/contract.ts. */
  origin: string;
  /** The archive-scope key (the account axis). */
  scope: string;
  store: BackfillStore | null;
  http?: HttpPort;
  /**
   * 🔴 A test seam, in the same family as http/clock/pace: swap the plan table.
   * **Production code never sets it** — neither of background.ts's two call sites
   * (kickBackfill / runAlarmTick) has this field, so on the wire it is always
   * backfillPlanFor and the permitted set has not grown at all.
   * It exists for exactly one reason: C23 has to prove "the channel can send a
   * POST", and the production table **deliberately** still has no POST platform
   * (kimi/gemini's parameters still have no source, and filling them in would be
   * inventing them).
   */
  plans?: (platform: string) => import('./enumerate').BackfillEnumPlan | null;
  clock?: Clock;
  pace?: BackfillPace;
  /**
   * 🔴 W16 · The source of randomness for this run's jittered gaps and for the
   * day's cap draw.
   *
   * A test seam in the same family as `clock` / `pace` / `http`: omitted ⇒ the
   * production `Math.random` (`systemRandom`), which is what both of
   * background.ts's call sites do — neither sets this field, so nothing about
   * the shipped behaviour is decided by a test. Injecting `() => 0` makes every
   * jittered value land exactly on its documented minimum and `() => 1` on its
   * documented maximum, which is how the boundary is asserted rather than
   * sampled.
   */
  random?: RandomFn;
  listLimit?: number;
  /** How many bodies this run fetches at most; used to run in slices and to simulate "interrupted half way". */
  maxDetails?: number;
  /** Asked before every step whether to abort (simulates the browser closing / the SW being reclaimed). */
  shouldAbort?: () => boolean;
  /**
   * The archive exit: produces a CapturedFetch of exactly the same shape as the
   * live leg's, so the on-disk logic does not fork.
   *
   * 🔴 C20: the return value now **decides** — see SinkOutcome and sinkVerdict().
   *    This used to be `=> Promise<void> | void`, and the wiring did
   *    `await handleCaptured(c)` and then dropped the HandledResult on the floor,
   *    so "not stored" and "stored" were the same outcome in the debt ledger.
   */
  sink?: (captured: CapturedFetch) => Promise<SinkOutcome | void> | SinkOutcome | void;
}

export interface RunReport {
  stopped: StopReason;
  enumeratedPages: number;
  newDebts: number;
  archivedThisRun: string[];
  /** 🔴 C20: the items in this run whose body was fetched but could not be stored; already in the failure list and never retried. */
  failedThisRun: FailureEntry[];
  /** How many were blocked by "already archived ⇒ never enqueued again" during enumeration. */
  skippedAlreadyArchived: number;
  /** How many were already in the debt set and needed no second enqueue. */
  skippedAlreadyPending: number;
  /**
   * 🔴 C28: the named receipts for an empty body. Unverified-empty and
   * confirmed-legitimately-empty cannot share queue-empty; each one also carries
   * its own `complete` for the body item, and the former's must be false.
   */
  detailOutcomes: DetailOutcomeRecord[];
  progress: string;
  halted: HaltRecord | null;
  /**
   * 🔴 C26 · The named reason enumeration **could not finish**; null = no such thing.
   * When it is non-null, state.enumCursor.complete may still be true, but that means
   * "stopped here", not "everything has been listed". See EnumTruncation in
   * lib/backfill/types.ts.
   */
  enumTruncated: EnumTruncation | null;
  /** The milliseconds each gate() actually waited, kept separately for the two segments. */
  paceTrace: { enumerate: number[]; detail: number[] };
  state: BackfillState;
}

function isBackfillState(value: unknown): value is BackfillState {
  if (!value || typeof value !== 'object') return false;
  const s = value as Partial<BackfillState>;
  return (
    s.v === BACKFILL_STATE_VERSION &&
    Array.isArray(s.pending) &&
    Array.isArray(s.archived) &&
    typeof s.enumCursor === 'object'
  );
}

export async function loadState(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<BackfillState> {
  const raw = await store.load(stateKey(platform, scope));
  // A version mismatch starts a fresh empty set rather than forcing the old structure to fit.
  if (!isBackfillState(raw)) return initialState(platform, scope);
  // C28: v1 states have no such column; reading one back fills in an empty array, so
  // every later write-back keeps the empty-body outcomes together with pending.
  // The `optional` is only to accommodate old states' TypeScript shape.
  raw.detailOutcomes ??= [];
  return raw;
}

async function persist(store: BackfillStore, state: BackfillState): Promise<void> {
  await store.save(stateKey(state.platform, state.scope), state);
}

/** Classify a non-2xx: rate-limit family vs everything else. Both stop, but they leave different traces. */
function haltReasonForStatus(status: number): HaltReason {
  if (status === 429 || status === 403 || status >= 500) return 'rate-limited';
  return 'shape-changed';
}

export async function runBackfill(opts: BackfillOptions): Promise<RunReport> {
  const clock = opts.clock ?? systemClock;
  const pace = opts.pace ?? DEFAULT_PACE;
  const http = opts.http ?? notWiredHttp;
  const listLimit = opts.listLimit ?? DEFAULT_LIST_LIMIT;
  // 🔴 W16 · One draw source for the whole run, so "which number came from
  //    where" is answerable and a test can swap all of it with one function.
  const random = opts.random ?? systemRandom;

  // 🔴 C19: the Pacer construction moved after loadState — it now needs
  // state.lastFetchAt as its seed. The branch with no store never ran a single
  // request, so its wait sequence is necessarily empty.
  const emptyTrace = { enumerate: [] as number[], detail: [] as number[] };

  if (!opts.store) {
    // No persistence ⇒ no stop-and-resume ⇒ not allowed to run. The trace can only go to the log (there is nowhere to store it).
    const state = initialState(opts.platform, opts.scope);
    state.halted = {
      reason: 'storage-unavailable',
      at: clock.now(),
      detail: 'browser.storage.local unavailable; refusing to run without a resumable debt set',
    };
    console.warn('[chat-stasher] backfill halted: storage-unavailable');
    return {
      stopped: 'halted',
      enumeratedPages: 0,
      newDebts: 0,
      archivedThisRun: [],
      failedThisRun: [],
      skippedAlreadyArchived: 0,
      skippedAlreadyPending: 0,
      detailOutcomes: state.detailOutcomes ?? [],
      progress: formatProgress(state),
      halted: state.halted,
      enumTruncated: null,
      paceTrace: emptyTrace,
      state,
    };
  }

  const store = opts.store;
  const state = await loadState(store, opts.platform, opts.scope);

  // 🔴 C19 · BUG-3: seed both pacers with the persisted "moment of the last fetch",
  // so the interval takes effect across ticks.
  // Old sets have no such field ⇒ null ⇒ byte-identical to C11.
  const anchors = state.lastFetchAt ?? { enumerate: null, detail: null };
  const enumPacer = new Pacer(pace.enumerate, clock, 'enumerate', anchors.enumerate, random);
  const detailPacer = new Pacer(pace.detail, clock, 'detail', anchors.detail, random);
  /** Write the moment a segment was just let through back into state (persisting is each caller's own job). */
  const anchor = (segment: 'enumerate' | 'detail', at: number | null): void => {
    state.lastFetchAt = { ...(state.lastFetchAt ?? { enumerate: null, detail: null }), [segment]: at };
  };

  const archivedThisRun: string[] = [];
  const failedThisRun: FailureEntry[] = [];
  let enumeratedPages = 0;
  let newDebts = 0;
  let skippedAlreadyArchived = 0;
  let skippedAlreadyPending = 0;
  let enumTruncated: EnumTruncation | null = state.enumCursor.truncated ?? null;
  const detailOutcomes = state.detailOutcomes ?? (state.detailOutcomes = []);

  /** C28: an empty body outcome is written into the same ledger first, before deciding whether to stop or to settle on a confirmed result. */
  const recordDetailOutcome = (
    id: string,
    outcome: DetailOutcomeRecord['outcome'],
    at: number,
  ): DetailOutcomeRecord => {
    const entry: DetailOutcomeRecord = outcome === 'detail-empty-unverified'
      ? { sessionId: id, outcome, complete: false, at }
      : { sessionId: id, outcome, complete: true, at };
    detailOutcomes.push(entry);
    return entry;
  };

  /**
   * 🔴 C26 · The single exit for "enumeration cannot read any further".
   *
   * It does three things, none optional: stop the cursor (complete=true, so it stops
   * hammering the platform in a spin), leave a named mark **on the persisted ledger**
   * (enumCursor.truncated), and report it in this run's RunReport.
   * complete=true here does **not** mean "everything has been listed" — truncated is
   * exactly what distinguishes the two. Without it, "backfilled only as far as the
   * first page" and "there was only ever this one page" would look identical in the
   * ledger.
   */
  const stopEnumerating = (why: EnumTruncation, opts: { complete?: boolean } = {}): void => {
    // C26's missing cursor/has_more still stops the loop here and sets complete=true,
    // because that is the old state contract's "do not spin" marker; `truncated` is
    // what says it is not a complete enumeration.
    // C27's Perplexity empty/short page is stricter: it does not even have an API
    // termination field, only a client inference, so complete must stay false. The
    // loop looks at truncated as well, so this "not confirmed complete" state is not
    // mistaken for one that may keep sending requests or has already finished.
    if (opts.complete !== false) state.enumCursor.complete = true;
    state.enumCursor.truncated = why;
    enumTruncated = why;
    console.warn(
      `[chat-stasher] backfill enumeration stopped early: ${why} —`
      + ` enumerated ${state.enumCursor.offset} conversation(s) so far; this is NOT "no more history"`,
    );
  };

  /**
   * 🔴 W13 · **The consecutive-transient-failure streak carried across a resume.**
   *
   * It has to be a variable local to this run rather than a field read straight off
   * the state, because the state's halt record is cleared a few lines below when the
   * backoff has expired — and the whole point is that the *next* failure continues
   * the ladder rather than restarting at 1 (otherwise the backoff is not a backoff:
   * a platform failing every 20 minutes would be retried every 20 minutes forever).
   * A run that ends with no transient halt simply never re-writes it ⇒ reset.
   */
  let transientStreak = 0;

  const halt = async (reason: HaltReason, detail: string): Promise<RunReport> => {
    const at = clock.now();
    // 🔴 W13 · A transient reason gets a "when may we try again" and a streak count;
    //    a permanent one is written exactly as before (no `retryAt`, no `attempts`),
    //    which is what keeps the permanent semantics byte-identical.
    if (haltClassOf(reason) === 'transient' && isTransientReason(reason)) {
      transientStreak += 1;
      state.halted = {
        reason,
        at,
        detail,
        attempts: transientStreak,
        retryAt: at + transientRetryDelayMs(reason, transientStreak, random),
      };
    } else {
      state.halted = { reason, at, detail };
    }
    await persist(store, state);
    // Only the technical detail is logged, never a conversation body.
    console.warn(
      `[chat-stasher] backfill halted: ${reason} — ${detail}`
      + (state.halted.retryAt === undefined
        ? ''
        : ` (attempt ${state.halted.attempts}; retrying on its own after ${state.halted.retryAt - at} ms)`),
    );
    // 🔴 The two classes report different stop reasons: 'halted' says a human has to
    //    look, 'waiting-retry' says the leg will come back by itself. Collapsing them
    //    is what made a one-second glitch permanent.
    return report(haltClassOf(reason) === 'transient' ? 'waiting-retry' : 'halted');
  };

  const report = (stopped: StopReason): RunReport => ({
    stopped,
    enumeratedPages,
    newDebts,
    archivedThisRun,
    failedThisRun,
    skippedAlreadyArchived,
    skippedAlreadyPending,
    detailOutcomes,
    // 🔴 W13: the engine's own clock, so a waiting round's "about M minutes" is
    //    measured from the same instant the backoff itself was decided on.
    progress: formatProgress(state, clock.now()),
    halted: state.halted,
    enumTruncated,
    paceTrace: { enumerate: enumPacer.waits, detail: detailPacer.waits },
    state,
  });

  // 🔴 W13 · A persisted halt is now **two different things**, and this is the line
  //    that had them as one. Before: any recorded stop refused every later run until
  //    a human cleared `halted` by hand — and nothing in the product ever cleared it,
  //    so one `transport-error` (a page reload tearing the message channel, measured
  //    on a real account) froze that platform's backfill permanently. 7,391 debts,
  //    0 archived, for over an hour, with nothing wrong with the account.
  //
  //    · permanent ⇒ exactly the old behaviour, and it still needs a human.
  //    · transient ⇒ wait out the backoff, then clear the record and **carry on from
  //      the same cursor and the same debt set**. Nothing is written off: `pending`
  //      and `archived` are not touched on any path through this branch.
  if (state.halted) {
    if (haltClassOf(state.halted.reason) === 'permanent') return report('halted');

    // 🔴 A record written before W13 has no `retryAt` and is read as **due now** —
    //    "no delay was ever decided" is not "wait forever". That is precisely the
    //    state the real account was stuck in (reason 'transport-error', detail
    //    starting `list offset=`), and it has to come back on its own.
    const retryAt = state.halted.retryAt;
    if (retryAt !== undefined && clock.now() < retryAt) {
      // 🔴 No request, and no write either: a waiting round must be free. Returning
      //    the persisted record lets the popup say which attempt this is and when
      //    the next one comes — 'waiting-retry' is neither 'ran' nor 'halted'.
      return report('waiting-retry');
    }

    // Due: the streak continues across the resume, so the ladder does not restart.
    transientStreak = state.halted.attempts ?? 1;
    state.halted = null;
    await persist(store, state);
    console.warn(
      `[chat-stasher] backfill resuming after a transient stop`
      + ` (attempt ${transientStreak}; the debts were never touched)`,
    );
  }

  const platformRow = getPlatformByOrigin(opts.origin);
  if (!platformRow) {
    return halt('shape-changed', `origin ${opts.origin} is not in the platform table`);
  }

  // 🔴 C22 · **Before issuing any request**, ask: have we actually written this
  // platform's enumeration at all?
  //
  // This question did not exist before, and the engine built the URL straight from
  // ChatGPT's listPageUrl(origin) — so a DeepSeek user would have
  // `https://chat.deepseek.com/backend-api/conversations` fired at their own
  // account, get a 404 back, and leave a 'shape-changed' row in the ledger.
  // That trace was an **accurately worded lie**: the API had not changed, we had
  // simply never written it.
  const plan = (opts.plans ?? backfillPlanFor)(platformRow.id);
  if (!plan) {
    const gap = unsupportedBackfillFor(platformRow.id);
    // In the platform table but registered in neither ⇒ the wiring missed it, and that has to be sayable too.
    const detail = gap
      ? `platform ${platformRow.id} has no backfill enumeration yet; missing: ${gap.missing.join(' | ')}`
      : `platform ${platformRow.id} is in the platform table but is registered in neither`
        + ' BACKFILL_PLANS nor BACKFILL_UNSUPPORTED (lib/backfill/enumerate.ts)';
    return halt('unsupported-platform', detail);
  }

  // ---- Segment one: enumeration (cheap; one page per tick when bodies follow) ----
  //
  // 🔴 W10 · This segment and segment two are now **interleaved per tick**; see
  //    `listPagesThisTick` below. Before that they were strictly sequential: the
  //    list had to be finished in full before the first body was fetched, which
  //    on a heavy account meant minutes-to-hours of "archiving" with an empty
  //    archive, and it took a browser restart or an SW reclaim to make it worse.
  //
  // 🔴 C26: two paging schemes exist here, and **the plan decides**, not the
  // response content on the fly:
  //   · listCursorUrl declared     ⇒ cursor paging (DeepSeek: count + before_seq_id);
  //   · listCursorUrl not declared ⇒ offset paging (ChatGPT, byte-identical to C22).
  const cursorMode = plan.listCursorUrl !== undefined;
  /**
   * 🔴 W10 · **How many list pages one tick may read.**
   *
   * The defect this answers, as measured on a real account: 7,391 distinct
   * conversations, 50 minutes with backfill on, and **not one body fetched**.
   * The list segment was a loop that only ended when the whole list was read,
   * and the body segment sat *after* it — so on a heavy account the first body
   * waited for the entire list (74 pages at the default limit), and every
   * interruption (MV3 reclaiming the SW, `shouldAbort`) put the body segment's
   * entrance off to the next tick. Not a hang: a structure.
   *
   * So: a plan that can fetch bodies reads **at most one page per tick**, and
   * the body budget for that same tick is spent right after it. The next tick
   * carries on from the persisted cursor. Nothing about pacing changed — one
   * page per tick is *fewer* requests per tick than the old loop, never more.
   *
   * 🔴 Why `canBackfillDetail` and not simply 1 for everybody: a plan with no
   *    body segment (Perplexity, detailPath/detailUrl both null) has no body
   *    fetch that a long list could starve. Capping it at one page would gain
   *    nothing and would **lose** something: that plan's tick ends in
   *    halt('detail-unsupported'), and a persisted halt stops every later tick
   *    from reading page 2 at all — the list would be cut off at the first page
   *    and the halt could never be reached with more than one page on disk.
   *    So for list-only plans the list segment keeps running to the end of the
   *    list, byte-for-byte as before (tests/c27-pplx.test.ts pins that path).
   */
  const listPagesThisTick = canBackfillDetail(plan) ? 1 : Number.POSITIVE_INFINITY;
  let listPagesFetched = 0;
  // The trace has to say which page it stopped on. Under cursor mode `offset` is the
  // **number enumerated so far**, not a request parameter, so the two modes must be
  // worded differently — a trace that reads correctly but points at the wrong thing
  // is worse than none.
  const listWhere = (): string =>
    cursorMode
      ? `list cursor=${state.enumCursor.cursor ?? 'first-page'} (enumerated ${state.enumCursor.offset})`
      : `list offset=${state.enumCursor.offset}`;
  while (
    !state.enumCursor.complete
    && state.enumCursor.truncated === undefined
    // 🔴 W10: at most `listPagesThisTick` pages here, then the body segment below
    //    gets its turn within the same tick. The bound is on **pages per tick**,
    //    not on the page itself: everything inside this loop is unchanged.
    && listPagesFetched < listPagesThisTick
  ) {
    if (opts.shouldAbort?.()) return report('aborted');
    await enumPacer.gate();
    anchor('enumerate', enumPacer.lastAt);
    const url = cursorMode
      ? plan.listCursorUrl!(opts.origin, state.enumCursor.cursor ?? null, listLimit)
      : plan.listUrl(opts.origin, state.enumCursor.offset, listLimit);
    // 🔴 C23: the body can only come from the plan's own builder
    //    (listRequestInit → spec.body). No path lets the page side or the message
    //    side decide what is sent here.
    const init = listRequestInit(plan, opts.origin, state.enumCursor.offset, listLimit);
    let res: HttpResponse;
    try {
      res = await sendVia(http, url, init);
    } catch (err) {
      return halt('transport-error', `${listWhere()}: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(
        haltReasonForStatus(res.status),
        `${listWhere()} returned HTTP ${res.status}`,
      );
    }
    const parsed = plan.parseListPage(res.text);
    if (!parsed.ok) {
      return halt('shape-changed', `${listWhere()}: ${parsed.detail}`);
    }
    enumeratedPages += 1;
    listPagesFetched += 1;

    // 🔴 W10 · The total the API gave is recorded, but it is **not** trusted as
    //    the denominator forever: it is a number this endpoint prints, not a
    //    measurement of how many conversations the account has (measured on a
    //    real account: `total = 901` while 7,391 distinct ids came back from
    //    this very endpoint). So it is kept as-is — 'response-total' is still
    //    the only value that can produce a percentage — and the page below
    //    disproves it the moment the rows actually listed outnumber it.
    //    🔴 Once disproved it is never trusted again: a fresh `total` from a
    //    later page does not overwrite 'contradicted', because a total that
    //    moves is itself evidence about how much the number is worth.
    if (parsed.page.total !== null) {
      state.totalKnown = parsed.page.total;
      if (state.totalSource !== 'contradicted') state.totalSource = 'response-total';
    }

    // First measure how much of this page is "already settled" — the direct evidence for "fetch nothing twice".
    const archivedSet = new Set(state.archived);
    const pendingSet = new Set(state.pending);
    for (const id of parsed.page.ids) {
      if (archivedSet.has(id)) skippedAlreadyArchived += 1;
      else if (pendingSet.has(id)) skippedAlreadyPending += 1;
    }
    newDebts += enqueueDebts(state, parsed.page.ids).length;

    // Offset mode normally advances by the number of rows read; Perplexity's three
    // sources state explicitly that the client does `offset += limit` itself, so
    // even when this page is short or empty, the persisted value keeps that request
    // stride. Under cursor mode `offset` remains only a count of how many have been
    // enumerated.
    state.enumCursor.offset += plan.platform === 'perplexity'
      ? listLimit
      : parsed.page.ids.length;

    /**
     * 🔴 W10 · **Disproving the total by measurement.**
     *
     * `total` is not the size of the account (measured: 901 reported, 7,391
     * distinct ids returned). The one thing that can be said about it with
     * evidence is that it has been **falsified** — and the moment for that is
     * exactly here: rows we actually hold in hand outnumber the number the API
     * says exist. From then on it is not a denominator (progress.ts refuses to
     * divide by it) and it is not a stopping condition (see the branch removed
     * at the end of this block).
     *
     * 🔴 Only the measured row count is compared — `enumCursor.offset` is "how
     *    many rows the list has handed us". Nothing is inferred from a field
     *    whose meaning is unknown; in particular the list response's
     *    `has_missing_conversations` is **not** read (its semantics have no
     *    source — the only public declaration of it, pionxzh's src/api.ts:330-339,
     *    annotates it with the author's own "// what is this for?").
     */
    if (state.totalSource !== 'contradicted'
      && state.totalKnown !== null
      && state.enumCursor.offset > state.totalKnown) {
      state.totalSource = 'contradicted';
      console.warn(
        '[chat-stasher] backfill: the list endpoint reported total='
        + `${state.totalKnown}, but ${state.enumCursor.offset} rows have already been listed;`
        + ' the total is not used as a denominator any more',
      );
    }

    if (parsed.page.ids.length === 0) {
      if (plan.platform === 'perplexity') {
        // 🔴 Perplexity has no known termination field such as has_more / total /
        // count. An empty page only means "nothing more was read this time", not the
        // API saying "that is the end"; it is traced separately from a short page.
        // DeepSeek takes the has_more branch below; the two platforms' rules must not
        // be conflated.
        stopEnumerating('empty-page-inferred', { complete: false });
      } else {
        state.enumCursor.complete = true;
      }
    } else if (cursorMode) {
      // 🔴 Cursor paging's termination test **recognises has_more only**.
      //    Never infer it from "this page has fewer than count ⇒ that is the end" —
      //    that would be treating an unknown as known.
      if (parsed.page.hasMore === undefined) {
        // The response carries no such boolean ⇒ we **do not know** whether more
        // follow. Stop, and leave a named trace.
        stopEnumerating('has-more-missing');
      } else if (parsed.page.hasMore === false) {
        state.enumCursor.complete = true;
      } else if (parsed.page.nextCursor === null || parsed.page.nextCursor === undefined) {
        // The API says there is another page, but this page could not supply a
        // cursor ⇒ it cannot page on.
        // 🔴 Only this page was backfilled, and that has to be said; never pretend
        // the whole thing was captured.
        stopEnumerating('cursor-missing');
      } else {
        state.enumCursor.cursor = parsed.page.nextCursor;
      }
    } else if (plan.platform === 'perplexity' && parsed.page.ids.length < listLimit) {
      // 🔴 C27 · This is the only "a short page means stop" branch. It is a client
      // inference all three sources share, not a termination signal supplied by the
      // API, so complete must not be set to true.
      // If it is ever confirmed that Perplexity returns a termination field, change
      // this branch: read that field, and set complete=true only when it is
      // explicitly false.
      stopEnumerating('short-page-inferred', { complete: false });
    }

    /**
     * 🔴 W10 · **The `offset >= totalKnown ⇒ complete` branch used to live here.
     *    It was removed, not loosened.**
     *
     * It read `state.enumCursor.offset >= state.totalKnown` and set complete=true.
     * On an account where the endpoint reported `total = 901` while really
     * holding 7,391 conversations, that line stops the listing at row 901 and
     * records complete=true — i.e. it writes "everything has been listed" over
     * "6,490 conversations were never even named". `enumCursor.truncated` cannot
     * catch it either: as far as that branch is concerned nothing went wrong.
     *
     * What terminates enumeration for offset paging now is the **empty page**
     * (`parsed.page.ids.length === 0`, the `else` branch above) — one real
     * observation ("this request came back with no rows"), the only stopping
     * signal all three reviewed implementations share
     * (pionxzh src/api.ts:610, pinguarmy src/contents/chatgpt-parser.ts:380-383,
     * wanda1416 packages/adapter-chatgpt/src/parse.ts:253).
     *
     * The price, written down rather than hidden: a full enumeration now costs
     * **one extra request** (the empty page at the end). What it buys is that
     * "the list is finished" is never said on the strength of a number this
     * endpoint has already printed wrongly.
     */
    await persist(store, state);
  }

  // 🔴 C26 · **Before issuing any body request**, ask: have we actually written this
  // platform's body segment at all?
  //
  // Perplexity is in exactly this intermediate state: the list segment has a
  // three-source provenance (conversations have been listed, debts are on disk), and
  // the body segment has none. (DeepSeek was in it until W8 filled its body segment
  // in; the branch, the halt reason and the wording below are unchanged — what moved
  // is which platform sits in it.) At that point:
  //  · it must not keep going — plan.detailUrl is null, and forcing it would mean
  //    inventing a body route on the spot;
  //  · and it must not quietly return 'queue-empty' either — that would amount to
  //    saying "it is all backfilled" when not one body was fetched.
  // So it is a **separate, persisted** halt: the debts stay untouched, and once the
  // body segment has a source, this batch of debts is simply worked through.
  //
  // 🔴 It halts only when there really are debts waiting. With none at all (the user
  //    genuinely has no history) nothing is being blocked by this half leg, and the
  //    normal 'queue-empty' is the right answer — otherwise "you have no history"
  //    would be written down as a stop record, exactly the confusion this repository
  //    most wants to avoid.
  if (!plan.detailUrl || !plan.detailPath) {
    if (state.pending.length === 0) return report('queue-empty');
    const gap = plan.partial?.missing.join(' | ') ?? 'detailPath / detailUrl';
    return halt(
      'detail-unsupported',
      `platform ${platformRow.id} can enumerate its conversation list`
      + ` (${state.pending.length} pending, ${state.archived.length} archived)`
      + ` but has no backfill detail route yet; missing: ${gap}`,
    );
  }
  const detailUrlOf = plan.detailUrl;

  // ---- Segment two: fetching bodies one by one (expensive, must be gentle) ----
  const today = dayKeyOf(clock.now());
  /**
   * 🔴 W16 · The day's cap is **drawn once, when the day rolls over**, and then
   *    persisted in `state.detailToday.cap` along with the counter.
   *
   * Why it is drawn here and nowhere else: this is the single point where the
   * day key changes, so there is exactly one moment a day at which a new cap can
   * be chosen. Every later run of the same day takes the `!==` branch as false
   * and therefore re-reads the stored cap — a restart cannot re-roll it, and in
   * particular cannot re-roll it *upward*.
   *
   * 🔴 `todaysCap` is `min(stored, plan)`, never the raw stored value: the
   *    plan's `maxPerDay` stays the hard ceiling, so a cap drawn at 200 can never
   *    exceed a caller that asked for less, and a hand-edited or corrupt stored
   *    value cannot raise the rate either. `maxPerDay: null` draws nothing.
   */
  if (state.detailToday.day !== today) {
    state.detailToday = { day: today, count: 0, cap: drawDailyCap(pace.detail.maxPerDay, random) ?? undefined };
    // 🔴 Persist the draw **before acting on it**. Without this line the very
    //    first run of a new day that stops early (the cap was already reached, or
    //    the budget was 0) would return without ever writing the counter, the
    //    stored day would stay yesterday's, and the next run would roll again —
    //    i.e. a restart loop could keep re-rolling upward until it hit 200. One
    //    write per platform-scope per local day buys the "drawn once, kept" rule.
    await persist(store, state);
  }
  const planCap = pace.detail.maxPerDay;
  const dailyCap = planCap === null
    ? null
    : Math.min(state.detailToday.cap ?? planCap, planCap);
  const budget = opts.maxDetails ?? Number.POSITIVE_INFINITY;

  while (state.pending.length > 0) {
    if (opts.shouldAbort?.()) return report('aborted');
    if (archivedThisRun.length >= budget) return report('budget-exhausted');
    if (dailyCap !== null && state.detailToday.count >= dailyCap) return report('daily-cap');

    const id = nextDebt(state);
    if (id === null) break;

    await detailPacer.gate();
    // 🔴 The moment is persisted **before the request goes out**. Body-fetching is
    // the segment that really has to be gentle: if the SW is reclaimed mid-fetch, or
    // the user closes the browser, the next tick still has to know "one was just
    // fetched", otherwise a restart becomes a back door around the interval.
    // The cost is one extra storage.local write per debt (about every 20 seconds),
    // which is negligible.
    anchor('detail', detailPacer.lastAt);
    await persist(store, state);
    const url = detailUrlOf(opts.origin, id);
    const init = detailRequestInit(plan, opts.origin, id);
    let res: HttpResponse;
    try {
      res = await sendVia(http, url, init);
    } catch (err) {
      // 🔴 A POST failure and a GET failure take **the same line**:
      //    halt('transport-error') + a persisted trace. No silent path was opened
      //    for POST.
      return halt('transport-error', `detail: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(haltReasonForStatus(res.status), `detail returned HTTP ${res.status}`);
    }
    // The same shape checker the live leg uses: if the API changes, this is the first to know.
    if (!matchesResponseShape(platformRow, res.text)) {
      return halt('shape-changed', `detail body does not match the ${platformRow.id} response shape`);
    }

    /**
     * 🔴 C28 · This is the single guardrail landing point for a future body parser:
     * after the shape is good and before the sink.
     *
     * No production plan has a parseDetailPage today, so this branch changes no
     * existing platform's behaviour and certainly does not guess body fields for
     * DeepSeek. Once the raw payload is available, the plan's parser implementation
     * should return "succeeded but empty" as detail-empty-unverified; this branch
     * then writes the receipt at once and halts, never letting the
     * sinkVerdict/settleDebt below pass it off as success. Only when a parser
     * explicitly returns detail-empty-confirmed may a legitimately empty
     * conversation be completed as this body item.
     */
    const detailParsed = plan.parseDetailPage?.(res.text);
    if (detailParsed?.ok === false) {
      return halt('shape-changed', `detail body: ${detailParsed.detail}`);
    }
    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-empty-unverified') {
      const entry = recordDetailOutcome(id, detailParsed.outcome, clock.now());
      return halt(
        'detail-empty-unverified',
        `detail returned HTTP ${res.status} with empty content for a pending conversation;`
        + ` recorded ${entry.outcome} with complete=false`,
      );
    }
    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-empty-confirmed') {
      recordDetailOutcome(id, detailParsed.outcome, clock.now());
      // A legitimately empty conversation is not a loss: it may be settled, but its
      // trace must be separate from the unverified empty above.
      settleDebt(state, id);
      archivedThisRun.push(id);
      state.detailToday.count += 1;
      await persist(store, state);
      continue;
    }

    const captured: CapturedFetch = {
      url,
      // 🔴 C23: write the method actually sent, faithfully. A GET segment is still
      // byte-for-byte 'GET' (init.method defaults to it).
      method: init.method,
      status: res.status,
      text: res.text,
      pageUrl: `${opts.origin}/c/${id}`,
      capturedAt: clock.now(),
      // 🔴 C21 · The root-cause fix's landing point: **the identity is expressed
      //    once.**
      //    This debt's id is the items[].id the list API gave (enumerate.ts:64-68),
      //    carried down here as-is, and the write-down path no longer scrapes it out
      //    of the URL a second time.
      //    ⇒ "two different debt keys collapsing onto one file name" is structurally
      //      impossible now: the file-name fragment = the debt key itself (the
      //      identity map; see pathSafeSessionId in contract.ts).
      sessionId: id,
    };
    // 🔴 C20 · This fix's landing point: **the sink's result decides.**
    //
    // The product owner's words: "losing something" and "losing something but
    // knowing about it" are two entirely different things, and this project exists
    // for the second one.
    //
    // This used to be `await opts.sink?.(captured); settleDebt(state, id);` —
    // without looking at whether the exit really stored the file, the debt was
    // struck off anyway, and that conversation was never tried again and nobody ever
    // knew it was lost. There are now two paths:
    //   stored ⇒ settleDebt (clear the debt, into archived)
    //   not stored ⇒ recordFailure (into the failure list; 🔴 no clearing, no
    //   archived, no retry)
    const verdict = sinkVerdict(await opts.sink?.(captured), id);

    if (!verdict.ok && !verdict.fatal) {
      // 🔴 W2 · The exit is temporarily unreachable (this machine's host is not
      //    there). This item **must not** be struck off: not into archived, not into
      //    the failure list, not out of pending.
      //    The request that really did go out still counts against today's quota
      //    (the count += 1 below), and then this leg stops at once — the next
      //    heartbeat asks the host whether it is there first.
      state.detailToday.count += 1;
      await persist(store, state);
      console.warn(`[chat-stasher] backfill paused: ${verdict.reason} — ${verdict.detail}`);
      return report('host-unavailable');
    }

    if (verdict.ok) {
      settleDebt(state, id);
      archivedThisRun.push(id);
    } else {
      // 🔴 Taken out of pending but **not** put into archived: no retry is a product
      //    decision, and pretending it was archived would make the progress numerator
      //    lie — neither may happen.
      dropDebt(state, id);
      failedThisRun.push(recordFailure(state, { id, reason: verdict.reason, at: clock.now() }));
      // Only the technical detail is logged: never a conversation body, never a full URL, never a full conversation id.
      console.warn(`[chat-stasher] backfill sink did not save: ${verdict.reason} — ${verdict.detail}`);
    }

    // Success or failure, it counts as one "body fetched today": the request really
    // did go out, and not counting it would open a back door around the daily quota
    // for failures.
    state.detailToday.count += 1;
    // Persist immediately after clearing a debt (or recording a failure) — the whole secret of stop-and-resume is this one line.
    await persist(store, state);
  }

  return report('queue-empty');
}
