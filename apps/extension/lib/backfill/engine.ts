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

import {
  currentReleaseChannel,
  getPlatformByOrigin,
  matchesResponseShape,
  type CapturedFetch,
  type ReleaseChannel,
} from '../contract';
import { runningBuildId } from '../extension-build';
import { isClaudeOrgId } from './claude-org';
import { dropDebt, enqueueDebts, nextDebt, settleDebt } from './debts';
import { recordFailure, type FailureEntry, type FailureReason } from './failures';
import {
  applyScope,
  backfillCapabilityOf,
  backfillPlanFor,
  canBackfillDetail,
  capabilityOf,
  detailRequestInit,
  listRequestInit,
  listTokenPostInit,
  unsupportedBackfillFor,
  DEFAULT_LIST_LIMIT,
  type BackfillRequestInit,
} from './enumerate';
import { countsOf, formatProgress } from './progress';
import { DEFAULT_PACE, Pacer, drawDailyCap, systemClock, type BackfillPace, type Clock } from './pace';
import { systemRandom, uniformBetween, type RandomFn } from './random';
import type { BackfillStore } from './store';
import { applyReenumerations, openLedger, recoverLedgerLoss, saveHeader, type Ledger } from './ledger';
import {
  dayKeyOf,
  haltClassOf,
  haltExpiredBecause,
  haltRetrySpent,
  haltSubjectOf,
  initialState,
  isTransientReason,
  transientRetryDelayMs,
  type BackfillState,
  type DetailOutcomeRecord,
  type EnumTruncation,
  type HaltJudgement,
  type HaltReason,
  type HaltRecord,
  type StopReason,
} from './types';

export interface HttpResponse {
  status: number;
  text: string;
  /**
   * 🔴 W64c · **Evidence, carried from the wrapper that owns the credential, never
   * inferred from the status.**
   *
   * `true` = the page-side wrapper for this platform's credential produced this
   * response from its credential path, after re-reading what the page holds. Absent =
   * nothing has been claimed, which is what a fixture, a platform with no credential,
   * and an older content script all produce — and the classifier reads it as "no
   * evidence about the credential", not as a denial.
   *
   * See `lib/platform-auth.ts`'s `GeminiAuthorizedResponse` for what a wrapper is
   * allowed to set, and `haltReasonForStatus` for who reads it.
   */
  survivedCredentialReread?: boolean;
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
 * 🔴 🔴 W59c · **How many list pages one alarm tick may read, for a plan with no
 * body segment.**
 *
 * This is the number that was `Infinity`, and `Infinity` is not a budget. W10 capped
 * the list segment at one page per tick *for plans that can fetch bodies*, so the
 * first body would not wait for the whole list — and deliberately left the list-only
 * plans uncapped, because their tick ends in `halt('detail-unsupported')`, and capping
 * them would cut the list off at page one while the halt held every later tick. The
 * reasoning behind that exception was sound and its conclusion was wrong: it read
 * "capping this plan is not free" as "this plan needs no cap", when what it needed was
 * a cap that does not truncate (the `budget-exhausted` ending below, which W59b added
 * for the re-decision tick and which is now the ending whenever a cap is what stopped
 * the loop). What was left was a leg whose tick reads the **entire** list in one
 * alarm wake: measured on the account W10 worked from, 74 pages — 74 requests in a
 * burst, from a page event, with the debts of the last 73 pages not even recorded
 * until their page arrives.
 *
 * 🔴 **Why 8.** Two existing numbers, from the two budgets this run already has:
 *   · the tick's **body** budget is `DEFAULT_TICK_DETAILS = 1` (lib/backfill/
 *     schedule.ts), one body per wake at a 20-45 s paced gap (pace.ts);
 *   · one list page at the enumeration pace is 2-6 s (pace.ts), so eight pages is
 *     16-48 s of paced list traffic — the same order of magnitude as the single body
 *     this tick already pays for, and nine requests in total against the gentlest
 *     reference implementation's ≤50 per run.
 *   Eight also makes a whole account's list a bounded number of ticks rather than a
 *   burst: ChatGPT's 1,000 conversations ≈ 10 pages ⇒ two ticks; the 74-page account
 *   above ⇒ ten ticks, about an hour of the 5-10 minute jittered cadence, against
 *   7,391 bodies it can only fetch 200 of a day. Enumeration is never the constraint
 *   on this leg; a burst is the one thing it must not be.
 *
 * 🔴 The cursor is persisted at the end of every page (see the loop), so a capped tick
 *    loses nothing: the next tick continues from the same page boundary, and
 *    `enumCursor.complete` keeps exactly the meaning it had — set only by a page that
 *    is the last one, never by the cap.
 */
export const LIST_PAGES_PER_TICK = 8;

/**
 * 🔴 W92b · **How many empty bodies in a row make an empty body stop being a
 * per-conversation fact and start looking like a contract change.**
 *
 * C28's whole point is that one empty body is not proof of anything: an
 * opened-but-never-sent conversation really is empty on Claude and Perplexity, and
 * treating that one conversation as a leg-wide stop left the id at the head of
 * pending (FIFO) — so the next run halted on it again and the platform archived
 * nothing (W92 §Task 5). So a single empty body is now the per-conversation
 * outcome `detail-empty`: the debt leaves pending, a failure receipt names it, and
 * the leg moves to the next conversation.
 *
 * But a body that suddenly parses to nothing for *every* conversation is the
 * contract change C28 was written for, and losing that signal would be worse than
 * the halt it fixes. K is the line between the two:
 *
 * 🔴 **Why 3.** It is the smallest number that cannot be reached by the known
 *    benign shape: a user can leave several conversations opened-but-never-sent
 *    (the measured Claude account had one such conversation at the head), and 1 or
 *    2 consecutive empties must not stop the leg. Three in a row, with no
 *    non-empty body between them, is not a run of unlucky conversations: it is a
 *    whole endpoint answering empty, which is what a changed wire looks like from
 *    this side. A non-empty body resets the streak, so a legitimate empty between
 *    two real bodies never accumulates.
 */
export const DETAIL_EMPTY_HALT_STREAK = 3;

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
   * POST", and at that point the production table **deliberately** had no POST
   * platform (filling one in would have been inventing it). 🔴 Later changes added
   * real POST plans by evidence — Perplexity's list, Grok's second detail step,
   * and W22's Kimi, whose list cursor and conversation id both travel in request
   * bodies — so today this seam is a way to reach a plan shape the table does not
   * ship, not a way to reach POST at all.
   */
  plans?: (platform: string) => import('./enumerate').BackfillEnumPlan | null;
  /**
   * 🔴 W59 · **Which extension build this run is**, for the halts it writes and for
   * judging the ones already stored. Omitted ⇒ `runningBuildId()` — the manifest
   * version the browser itself holds — so **production sets nothing** and the
   * stamp cannot drift from what is installed.
   *
   * A seam in the same family as `plans`: a test that wants to be "a different
   * build" or "a build that cannot name itself" says so here rather than by
   * editing the manifest. `null` means "this build cannot be named", which the
   * reader answers conservatively (see `HaltJudgement`) — it is a value, not a
   * missing one, and it is why this is `string | null` rather than `string`.
   */
  build?: string | null;
  /**
   * 🔴 W91 · **Which release channel this run is judged in.**
   *
   * The platform table is already channel-filtered at import time, so production
   * never sets this: an experimental origin simply is not in `PLATFORMS` in a
   * stable build, and this run halts on it by name. The field exists for the one
   * caller that cannot reproduce that: the test suite pins the build-time channel
   * to `dev`, so a test that wants to prove "stable never serves Perplexity/Kimi"
   * says `channel: 'stable'` here instead of rebuilding the bundle.
   */
  channel?: ReleaseChannel;
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

/**
 * Read one scope's state, migrating the pre-W18 record if that is what is there.
 *
 * 🔴 W18 · This used to be "read the one key, or start from empty if it did not
 *    parse". It is now three outcomes that must not be collapsed (CLAUDE.md
 *    invariant 1): nothing recorded (⇒ an empty set, which is a measurement), a
 *    readable record (⇒ its content), and **a record we cannot read** (⇒ a refusal
 *    that writes nothing and touches nothing — never an empty set).
 *
 * The debt ids no longer live at this key; `lib/backfill/ledger.ts` owns the split
 * and the migration. This function is the read-only convenience wrapper the tests
 * and the caller use when they want the assembled state and nothing else.
 */
export async function loadState(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<BackfillState> {
  const opened = await openLedger(store, platform, scope);
  if (opened.ok) return opened.state;
  // Refused: hand back a carrier that carries the refusal and **no** debt ids. The
  // caller decides what to do; it is never silently an empty debt set, because
  // nothing here is written and no empty set is handed out under the real key.
  const carrier = initialState(platform, scope);
  carrier.halted = { reason: opened.refusal.reason, at: Date.now(), detail: opened.refusal.detail };
  return carrier;
}

/**
 * 🔴 W31c · **Write down a named stop that happened before the leg made a single
 * request**, so the popup can say why instead of showing silence.
 *
 * ## Why this is not "just run the leg and let it halt"
 *
 * For claude.ai the organization is resolved *outside* the run: the background
 * asks the page (lib/backfill/tab-port.ts's `askTabForClaudeOrg`), because only
 * the page has the cookie and the same-origin `fetch`. So the refusal is reached
 * by code that never entered `runBackfill` — and without this function its only
 * outcome would be "no target registered", which the user reads as *nothing
 * happened*. The four reasons are facts about the account and each has a
 * different remedy; dropping them on the floor would be the same class of mistake
 * as recording an empty result for an unknown one.
 *
 * ## What it writes, and what it deliberately does not
 *  · **the header only** (`saveHeader`): no debt id moves, no counter moves, no
 *    cursor moves. This is a note about why the leg is not running, not progress;
 *  · **nothing at all** when the record cannot be read (`openLedger` refuses) —
 *    the same rule `runBackfill` follows, because writing over a record we could
 *    not read is the failure W18 exists to prevent;
 *  · **false** when there is no store: the caller is told the note was not
 *    written, rather than being told it was.
 *
 * 🔴 The streak rule is `runBackfill`'s, read from the **persisted** record rather
 *    than from a run-local counter, and that difference is forced by where this
 *    runs: a resolver failure is one tick's whole event, so "consecutive" can only
 *    mean "the record that was already there says the same reason". A transient
 *    record therefore keeps counting up across ticks (5 → 15 → 30 → 60 minutes,
 *    `transientRetryDelayMs`), and a run that stops for another reason resets it
 *    simply by writing its own record over this one.
 */
export async function recordBackfillHalt(
  store: BackfillStore | null,
  opts: {
    platform: string;
    scope: string;
    reason: HaltReason;
    detail: string;
    /** Test seams, in the same family as runBackfill's: omitted ⇒ the real clock and Math.random. */
    clock?: Clock;
    random?: RandomFn;
    /** 🔴 W59 · Same meaning and same default as `BackfillOptions.build`: omitted ⇒ the running build. */
    build?: string | null;
  },
): Promise<boolean> {
  if (!store) return false;
  const opened = await openLedger(store, opts.platform, opts.scope);
  if (!opened.ok) return false;
  const at = (opts.clock ?? systemClock).now();
  const state = opened.state;
  /**
   * 🔴 W44 · The same marker rule as `runBackfill`'s halt funnel, for the same
   * reason and with one honest difference: this function is reached from the
   * background's resolver path, where no plan lookup is injected, so it asks the
   * **production** table (`backfillCapabilityOf`). The three reasons it is called
   * with today — the two organization facts and a transport error — are all
   * non-capability, so `haltSubjectOf` returns `{}` here and every record it writes
   * is byte-identical to what it wrote before W44. The line is here rather than
   * omitted so that a future caller reaching it with a capability reason cannot
   * write a record the engine will never expire.
   */
  const capabilityMark = haltSubjectOf(opts.reason) === 'capability'
    ? { capability: backfillCapabilityOf(opts.platform) }
    : {};
  /**
   * 🔴 W59 · And the same build-stamp rule, for a sharper reason than symmetry: the
   *    two organization halts are written **here**, not in a run, and one of them
   *    (`org-ambiguous`) is exactly the kind whose truth condition can change
   *    without this build doing anything — the user opens a conversation in the
   *    organization they meant, the page shows it, and the resolver that refused
   *    yesterday would answer today. Without a stamp this record could not be told
   *    from this build's own, and the popup's instruction ("open a conversation
   *    once") would be advice the leg never acts on.
   */
  const build = opts.build !== undefined ? opts.build : runningBuildId();
  const buildMark = build === null ? {} : { build };
  if (haltClassOf(opts.reason) === 'transient' && isTransientReason(opts.reason)) {
    const previous = state.halted;
    const streak = (previous?.reason === opts.reason ? (previous.attempts ?? 0) : 0) + 1;
    state.halted = {
      reason: opts.reason,
      at,
      detail: opts.detail,
      attempts: streak,
      retryAt: at + transientRetryDelayMs(opts.reason, streak, opts.random ?? systemRandom),
    };
  } else {
    state.halted = { reason: opts.reason, at, detail: opts.detail, ...capabilityMark, ...buildMark };
  }
  // 🔴 W59b · A refusal written over an attempt is the verdict on that attempt, so the
  //    marker goes with it — the same rule `runBackfill`'s `halt()` follows, and for
  //    the same reason: a marker left behind would spend a *future* attempt on a
  //    question that has already been answered on disk.
  state.haltRetried = undefined;
  await saveHeader(store, state);
  return true;
}

/**
 * Classify a non-2xx for any backfill segment: credential refusal, rate-limit
 * family, or everything else. All three stop, but they leave different traces and
 * they promise different things.
 *
 * 🔴 W64 · **A 401 is its own answer, and it is not "the shape changed".**
 *
 * Measured 2026-09-23 from the page's own context in a logged-in Chrome: the list
 * request `KIMI_PLAN` builds — the plan's own body, every header
 * `createKimiAuthorizedFetch` produces, `authorization: Bearer <the page's
 * localStorage access_token>` — answered **HTTP 401** with
 * `{ code: "unauthenticated", message: "invalid user token: token has invalid claims:
 * token is expired" }`. The stored token had **expired about 15 hours earlier**; the
 * key name had not moved and no header was missing (the W64 probe table is in that
 * task's report). The cookie-only form of the same request answered 401 too, with
 * `code: "unauthenticated"` and no `message`.
 *
 * What a user was told before this line existed: `shape-changed` — "the API changed,
 * wait for a fix". That is a **permanent** record, so the leg never asked again, and
 * nothing in the product clears one. The platform had said, in as many words, that
 * the credential it was handed is expired.
 *
 * 🔴 **Why every 401, and not only the platforms that have an auth wrapper.**
 *    `lib/platform-auth.ts` is the only thing that knows which platforms send a
 *    credential, and a second list of them living here is a list that can disagree
 *    with it — silently, and in the direction that keeps this bug, because the wrong
 *    answer is still a *permanent* record. The reason itself needs no such list: a
 *    401 says a credential was required and what was presented was not accepted, and
 *    there is no platform anywhere for which that is a statement about the wire
 *    format. That is also why 401 may not fall to `shape-changed` on the one plan
 *    that declares no credential — the sentence would be false there too.
 *
 * 🔴 **Why not `rate-limited`.** It is a different fact about why the platform said
 *    no, its sentence promises a wait rather than a login, and it sits on its own
 *    rung. `auth-refused` is the existing reason for a refused credential; it is
 *    transient (`haltClassOf`), which is what makes the leg come back on its own and
 *    pick up a token the page has since refreshed — and refreshing is the whole
 *    remedy, since the token is re-read on every request (`lib/platform-auth.ts`).
 *    Its popup sentence names the login, which is the correct action for a 401.
 *    🔴 The cost, stated rather than implied: an account that is logged out for good
 *    now costs one request per rung (30 min, then 2 h) instead of stopping at one.
 *    W61b already made exactly this trade for the same reason and for the same
 *    reason class — a permanent record froze a platform across logins, and nothing
 *    in the product clears one.
 *
 * 🔴 **403 is deliberately left where it is**, and this is a decision rather than an
 *    omission. 403 stays `rate-limited`: 429/403/5xx-as-"not now" is the reading the
 *    ladder below was built on, the task that added this line forbids moving it
 *    without evidence, and there is still none — no platform this leg drives has been
 *    measured answering 403 for a credential reason. 400 is no longer a blanket
 *    `shape-changed`; see W64b below for the one platform where it is not one.
 *
 * 🔴 🔴 W64b · **A 400 on Gemini is the same login refusal a 401 is, and Gemini is the
 *    only platform for which that is claimed.**
 *
 *    Gemini's wrapper records the measurement this rests on (`createGeminiAuthorizedFetch`'s
 *    header, from the 2026-09-14 logged-in probe): a `batchexecute` request whose `at`
 *    is missing or stale is answered **HTTP 400** — "a real refusal, never data, never
 *    an empty page" — and that is why the wrapper re-reads `at` and retries **on 400**
 *    rather than only on 401. A retry keyed on a malformed-request status would be
 *    pointless; it is keyed on that status because that status is what this platform
 *    sends when the credential it was handed is not usable.
 *
 *    So a 400 that survives the wrapper's retry is the condition W64 handles for 401,
 *    and until W64b it landed on `shape-changed` — **permanent**, so a user who was
 *    signed out when the leg ran stayed stopped after signing back in, and nothing in
 *    the product clears such a record. It is now the transient `auth-refused`: the leg
 *    comes back on its own and picks up the `at` the page has since refreshed.
 *
 * 🔴 **Why the platform and not the status.** `platform` is the plan's own id, so the
 *    rule is "a 400 from Gemini", not "a 400". Every other platform keeps 400 ⇒
 *    `shape-changed`, because there is no measurement on them separating an auth
 *    refusal from a genuinely malformed request, and a blanket `400 → auth-refused`
 *    would swallow every malformed request into a message telling the user to sign in
 *    — a remedy for something that is not wrong with their account. Choosing a reason
 *    by evidence is the rule this whole function follows; this is what the evidence
 *    covers and no more.
 *
 * 🔴 🔴 W64c · **…and the evidence has to arrive, rather than be assumed from the
 *    status.** The paragraph above was written as if `platform === 'gemini'` *were* the
 *    evidence; it is not. The re-review of W64b found the rule applying to every Gemini
 *    400, including one nothing had been measured about (`nm/R64b-grok.log`, finding 2:
 *    "Any Gemini 400 is mapped, including a malformed batch"). The measurement belongs
 *    to an event — this platform refusing a request that came back through *its*
 *    credential path — and only the wrapper that owns that credential sees the event.
 *    So it is carried: `survivedCredentialReread` on the response, set by
 *    `createGeminiAuthorizedFetch` and by nothing else, threaded through the content
 *    script and the fetch channel (`lib/backfill/tab-port.ts`). A 400 without it is a
 *    400 with no evidence, and the honest answer for that is what every other platform
 *    gets.
 *
 * 🔴 **The residual, stated rather than implied.** A Gemini 400 caused by a genuinely
 *    malformed batch that *did* go out through the credential path is still read as a
 *    login refusal, because the status cannot separate the two — the same admission W64
 *    makes above about a 401 with no `message`. The cost is bounded and it is the
 *    cheaper error: the record is transient (30 min, then 2 h) rather than permanent,
 *    so the leg retries and records the same 400 again with its status in the trace,
 *    where the old behaviour would have stopped the platform for good on the first one.
 *    What W64c removes is the case where there is no evidence at all.
 */

/**
 * 🔴 W59b · **Spend this build's one re-decision on a scope's stored halt — before the
 * question is asked.**
 *
 * The other half of the bound. `resolveScopeForTick` decides whether to ask the page
 * for an organization, and the asking is the cost: with no organization on the page or
 * in the cookie, the resolver reaches `GET /api/organizations` (lib/backfill/
 * claude-page.ts). The refusal it produces is written *after* that request by
 * `recordBackfillHalt`, so a `saveHeader` that throws leaves the older build's record
 * in place, `scopeRetryDue` reads the same record as due again on the next tick, and
 * the endpoint is asked once per tick — the defect R59 measured.
 *
 * So the attempt is recorded first, in the header, under the same `haltRetried` field
 * the engine's funnel reads: one fact, two readers. The record is not touched. The
 * record is not even required to exist — which is the case this function exists for,
 * since a scope can be ticked before any run has written a record for it, and a marker
 * that lived inside `halted` would have had nowhere to go.
 *
 * 🔴 **`false` means "do not ask".** A stamp that cannot be written is exactly the case
 *    the bound is for, and the answer is to fail closed: the caller returns
 *    `UNRESOLVED_SCOPE` and issues nothing. The leg is not worse off — the engine
 *    still stops that scope by name — and the alternative, asking without the stamp,
 *    is the per-tick request this change exists to remove.
 */
export async function markScopeRetried(
  store: BackfillStore | null,
  opts: {
    platform: string;
    scope: string;
    /** 🔴 Same meaning and same default as `recordBackfillHalt`'s: omitted ⇒ the running build. */
    build?: string | null;
    clock?: Clock;
  },
): Promise<boolean> {
  if (!store) return false;
  const opened = await openLedger(store, opts.platform, opts.scope);
  if (!opened.ok) return false;
  const build = opts.build !== undefined ? opts.build : runningBuildId();
  // A build that cannot name itself has nothing to spend: `haltRetrySpent` answers
  // "not spent" for a null build, so a marker written here could never be read back
  // and the write would be a lie on disk. The caller still asks (this returns true),
  // which is the old behaviour exactly — an environment without a manifest gets the
  // pre-W59b semantics rather than a new refusal nobody asked for.
  if (build === null) return true;
  opened.state.haltRetried = { build, at: (opts.clock ?? systemClock).now() };
  try {
    await saveHeader(store, opened.state);
  } catch {
    // 🔴 The stamp did not land, so the bound is not in force — and asking without it
    //    is exactly the per-tick request this exists to remove. Refuse the question
    //    rather than lose the record of having asked it.
    // 🔴 The platform only: a scope is an account identifier on the scoped plans, and
    //    identifiers do not go into logs (the same rule the engine's own traces follow).
    console.warn(
      `[chat-stasher] scope resolution held: this build could not record its one`
      + ` re-decision for ${opts.platform}, so the account was not asked again`,
    );
    return false;
  }
  return true;
}

/** Classify a non-2xx: rate-limit family vs everything else. Both stop, but they leave different traces. */
function haltReasonForStatus(
  status: number,
  platform: string,
  /**
   * 🔴 W64c · The wrapper's own statement that this response came back from its
   * credential path — see `HttpResponse.survivedCredentialReread`. Required, not
   * optional, so that a new call site has to say which response it is holding instead
   * of defaulting into the wrong answer.
   */
  survivedCredentialReread: boolean,
): HaltReason {
  if (status === 401) return 'auth-refused';
  /**
   * 🔴 W64b · Gemini's own measured shape of "the token was missing or stale" — see
   *    this function's header.
   *
   * 🔴 W64c · **And the evidence is now required, rather than assumed from the
   *    status.** The re-review found this line classifying *every* Gemini 400 as a
   *    refused login, including one nothing was measured about — a malformed batch
   *    this leg built itself — while the message it produced told the user to sign in
   *    (`nm/R64b-grok.log`, finding 2). A 400 is a statement about the credential only
   *    when the wrapper that owns that credential says so: it read the page's `at`,
   *    re-read it once, and this response is what stood afterwards. With no such
   *    evidence the honest answer is the same one every other platform gets —
   *    `shape-changed`.
   *
   *    Scoped to the platform as well, and to that platform only: no other plan has
   *    such a measurement, so on every other plan a 400 is a malformed request, which
   *    is a wire fact and stays `shape-changed` whatever a response claims.
   *
   *    The residual, unchanged and stated rather than implied: a malformed batch that
   *    *does* go out through Gemini's credential path is still indistinguishable from a
   *    stale `at` at the status level, so it is still read as a login refusal. What the
   *    evidence rules out is the case where there is no evidence at all.
   */
  if (status === 400 && platform === 'gemini' && survivedCredentialReread) return 'auth-refused';
  if (status === 429 || status === 403 || status >= 500) return 'rate-limited';
  return 'shape-changed';
}

/**
 * 🔴 W45 · **Turn a `ledger-mismatch` refusal into the detail that says what happens next.**
 *
 * The refusal's own detail is the diagnosis (four numbers, and what was tried
 * before giving up). This appends the *prognosis*, because the two outcomes are
 * different facts about the user's history and neither may be left to be inferred
 * from the other:
 *
 *   · the scope was made listable again ⇒ say so, and say the honest cost. The ids
 *     are gone, so `archived` is gone with them: a conversation that had already
 *     been archived cannot be told from one that never was, and re-listing will
 *     enqueue it again. One duplicate body fetch, and **not** a lost conversation —
 *     but a user who was not told would read a second copy as the archive
 *     misbehaving;
 *   · it was not ⇒ say that too, and name why. "The leg is stopped and we could not
 *     prepare it to start again" is not the same sentence as "it will fix itself",
 *     and reporting the first as the second is what this whole task is about.
 *
 * The returned string is persisted (the run's halt record and the tick record), so
 * it carries counts and reasons only — never a conversation id, never a body.
 */
async function recoverAndSay(
  store: BackfillStore,
  platform: string,
  scope: string,
  detail: string,
  at: number,
): Promise<string> {
  const recovery = await recoverLedgerLoss(store, platform, scope, at);
  if (!recovery.ok) {
    return `${detail}. This scope was NOT prepared to be listed again (${recovery.why}: `
      + `${recovery.detail}), so it stays exactly as it is until that is fixed`;
  }
  return `${detail}. Its enumeration cursor has been reset, so the next run will read the conversation list again from`
    + ` the start; the ${recovery.loss.missing} id(s) it can no longer account for (of ${recovery.loss.recorded} recorded)`
    + ' could not be recovered, and an already-archived conversation cannot be told from one that was never fetched —'
    + ' it will be listed again and its body fetched a second time';
}

/**
 * 🔴 W92d · **Park an empty body's id instead of writing it off.**
 *
 * The id moves from the head of `pending` to the tail (so the FIFO moves on to the
 * next conversation) and is remembered in `state.parkedEmpty`. It is **still owed**:
 * it leaves the debt set only when a later real body proves the endpoint works (then
 * it is dropped with a `detail-empty` receipt) or when the guard halts at
 * `DETAIL_EMPTY_HALT_STREAK`, where every parked id stays in `pending`.
 *
 * 🔴 Why this replaced W92b's immediate `dropDebt`: R92b §2 measured that a dropped id
 *    is never retried — the enumeration cursor is already `complete`, so nothing
 *    re-enqueues it, and `recordFailure` keeps only a short id in a list the engine
 *    never reads. A single empty conversation is indistinguishable from the first
 *    body of a changed endpoint, so writing it off on sight is the exact loss this
 *    fix-back exists to stop.
 */
function parkEmpty(state: BackfillState, id: string): void {
  const at = state.pending.indexOf(id);
  if (at >= 0) {
    state.pending.splice(at, 1);
    state.pending.push(id);
  }
  const parked = state.parkedEmpty ?? (state.parkedEmpty = []);
  if (!parked.includes(id)) parked.push(id);
}

/**
 * Move a parked id to the tail again, issuing **no request**. Reached when a parked id
 * comes round to the head before any proof; it is the FIFO turning over, not a fetch.
 */
function rotatePendingToTail(state: BackfillState, id: string): void {
  const at = state.pending.indexOf(id);
  if (at < 0) return;
  state.pending.splice(at, 1);
  state.pending.push(id);
}

/** Is this id parked (an empty body waiting for either proof or the halt)? */
function isParkedEmpty(state: BackfillState, id: string): boolean {
  return (state.parkedEmpty ?? []).includes(id);
}

export async function runBackfill(opts: BackfillOptions): Promise<RunReport> {
  const clock = opts.clock ?? systemClock;
  const pace = opts.pace ?? DEFAULT_PACE;
  const http = opts.http ?? notWiredHttp;
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
      progress: formatProgress(countsOf(state)),
      halted: state.halted,
      enumTruncated: null,
      paceTrace: emptyTrace,
      state,
    };
  }

  const store = opts.store;

  // 🔴 W18 · Open the ledger **before anything else**. A state record that cannot be
  //    read is a refusal to run, not an empty debt set: running against an empty
  //    set would re-enumerate the whole account and re-fetch bodies that are
  //    already archived, and — worse — would write that empty set back over the
  //    record it could not read. Nothing is written on this path.
  const opened = await openLedger(store, opts.platform, opts.scope);
  if (!opened.ok) {
    const refused = initialState(opts.platform, opts.scope);
    refused.halted = {
      reason: opened.refusal.reason,
      at: clock.now(),
      detail: opened.refusal.detail,
    };
    if (opened.refusal.reason === 'ledger-mismatch') {
      // 🔴 W45 · **The refusal is not the end of the story, and leaving it there
      //    would be the same failure with a better name.** A scope whose debts were
      //    destroyed has `enumCursor.complete === true`, so it will never enumerate
      //    again: refusing forever would trade four hours of silence for a permanent
      //    one, with the same zero rows underneath. The repair is a one-shot header
      //    reset — reset the cursor, write the counts the store actually has — after
      //    which this condition cannot recur (see `recoverLedgerLoss`), and the next
      //    run reads the list again from the start.
      //
      //    This run still **fetches nothing**: the refusal stands, and it is what
      //    the caller, the tick record and the popup see.
      refused.halted.detail = await recoverAndSay(store, opts.platform, opts.scope, opened.refusal.detail, clock.now());
    }
    console.warn(`[chat-stasher] backfill halted: ${opened.refusal.reason} — ${refused.halted.detail}`);
    return {
      stopped: 'halted',
      enumeratedPages: 0,
      newDebts: 0,
      archivedThisRun: [],
      failedThisRun: [],
      skippedAlreadyArchived: 0,
      skippedAlreadyPending: 0,
      detailOutcomes: refused.detailOutcomes ?? [],
      progress: formatProgress(countsOf(refused), clock.now()),
      halted: refused.halted,
      enumTruncated: null,
      paceTrace: emptyTrace,
      state: refused,
    };
  }
  const { state, ledger } = opened;
  /** Persist the header, plus only the debt ids that actually moved (lib/backfill/ledger.ts). */
  const persist = async (s: BackfillState): Promise<void> => {
    await ledger.save(s);
  };

  /**
   * 🔴 W98 · **The one-time re-enumeration a parser fix owes a ledger that predates
   * it, and it happens before anything in this run reads the cursor.**
   *
   * A scope whose list was fully enumerated and whose bodies an old parser then
   * refused has `enumCursor.complete === true`; nothing reads the list again, and
   * each refused id was dropped, so those conversations are never backfilled. The
   * fix cannot reach them without re-reading the list once. `migrationsDue` /
   * `applyReenumerations` (lib/backfill/ledger.ts) decide that "once" by a marker
   * keyed by platform + migration id, and this is where the decision is taken.
   *
   * 🔴 It is applied **before the halt block below**, on purpose. The reset is a
   *    ledger migration, not a fetch: it is also owed on a scope that is currently
   *    stopped, and leaving it unmade because a halt returns early would mean the
   *    repair waits on a record a human controls. The marker is written in the same
   *    header write as the reset, so a run that stops right here cannot leave the
   *    migration half-done — and the cursor it reset is already what the next run
   *    reads.
   *
   * 🔴 Nothing is fetched by this write and no id is touched: `enqueueDebts` below is
   *    what brings back only the ids in neither `pending` nor `archived`, on whatever
   *    pages the ordinary list pacer and daily caps allow.
   *
   * 🔴 The log line names only the platform and the migration id. The scope is an
   *    account identifier and never goes into a log (see the repo's data rules).
   */
  const reenumerated = applyReenumerations(state, clock.now());
  if (reenumerated.length > 0) {
    await persist(state);
    for (const migration of reenumerated) {
      console.warn(
        `[chat-stasher] backfill re-enumerating ${migration.platform} once (${migration.id}):`
        + ' the enumeration cursor was reset so ids dropped before this fix can be listed again',
      );
    }
  }

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
   * 🔴 W92d · **The proof arrived: a real body was archived in this scope, so the
   * parked empties are finally written off and the streak resets.**
   *
   * This is the only place `detail-empty` becomes a conclusion rather than a parked
   * question, and the only place `state.emptyStreak` goes back to 0. The ids are
   * dropped with a receipt (no retry, the product decision `failures.ts` records),
   * and `state.parkedEmpty` is cleared in the same mutation so the persisted header
   * cannot hold ids that are no longer parked.
   *
   * 🔴 The request that produced the reply is counted once already; this function
   *    must **not** touch `detailToday.count` — dropping a parked id settles no new
   *    request, and counting one would open the daily-cap back door W16 closed.
   */
  const settleParkedEmpties = (at: number): void => {
    const parked = state.parkedEmpty ?? [];
    for (const parkedId of parked) {
      // Guard rather than assume: `parkedEmpty` is a persisted subset of `pending`,
      // but a hand-edited or half-migrated header must not make this drop an id that
      // is not owed (there would be nothing to drop, and the receipt would name a
      // conversation this run never saw).
      if (!state.pending.includes(parkedId)) continue;
      dropDebt(state, parkedId);
      failedThisRun.push(recordFailure(state, { id: parkedId, reason: 'detail-empty', at }));
    }
    state.parkedEmpty = [];
    state.emptyStreak = 0;
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

  /**
   * 🔴 W44 · **What this build can do, asked of the same lookup the judgement comes
   *    from.** `opts.plans` is the injected plan table (production never sets it),
   *    so a record written through the seam is judged against the very table the
   *    seam answered with — the marker and the halt cannot be computed from two
   *    different tables, which is what makes the expiry check sound rather than
   *    merely plausible.
   *    The platform is `state.platform`: the one this record is **stored under**, so
   *    the answer belongs to the same key as the record, whichever origin this run
   *    was handed.
   */
  const currentCapability = () => capabilityOf((opts.plans ?? backfillPlanFor)(state.platform));

  /**
   * 🔴 W59 · **Which build this is** — read once per run, from the manifest the
   * browser holds, so every record this run writes carries the same identity and
   * the record that judged a stored halt is the one that will be written back.
   *
   * 🔴 `null` is a real answer ("this build cannot name itself") and it is *not*
   *    turned into a string here. What a null stamp means to a reader is decided in
   *    `HaltJudgement`/`haltExpiredBecause`, in one place, rather than twice.
   */
  const currentBuild = opts.build !== undefined ? opts.build : runningBuildId();

  /**
   * 🔴 W44/W59 · **Everything a stored record is judged against**, as one value.
   * `opts.plans` is the injected plan table (production never sets it), so a record
   * written through the seam is judged against the very table the seam answered
   * with — the marker and the halt cannot be computed from two different tables,
   * which is what makes the capability check sound rather than merely plausible.
   * The platform is `state.platform`: the one this record is **stored under**, so
   * the answer belongs to the same key as the record, whichever origin this run
   * was handed.
   */
  const currentJudgement = (): HaltJudgement => ({
    capability: currentCapability(),
    build: currentBuild,
    // 🔴 W59b · The third fact has to travel with the other two, or the engine and
    //    the alarm's preflight could disagree about whether this build has already
    //    asked its question — which is the drift `haltExpiredBecause` exists to make
    //    impossible. Absent ⇒ no attempt has been spent here.
    retriedBy: state.haltRetried?.build,
  });

  /**
   * 🔴 W59b · **This run is the one re-decision of a stored halt, and it has not yet
   *    written its verdict.**
   *
   * The run-local half of the bound. The header half (`haltRetried`) is what makes the
   * attempt survive a run that dies; this flag is what makes the run **clear** the
   * record once it knows the answer — see `finish`, which is the only thing that reads
   * it, and `halt()`, which turns it off because the new record *is* the verdict.
   *
   * 🔴 Why it has to be a variable rather than a re-read of `state.haltExpired`: the
   *    same field is left behind by a capability expiry that was followed by a
   *    successful run, and by a build expiry from long ago. What this flag says is
   *    narrower and is the thing that matters — *this* run started by suspending a
   *    stored record, so *this* run owes a verdict.
   */
  let redecting = false;

  /**
   * 🔴 🔴 W59c · **Has this build already spent an attempt on this scope without a
   * verdict landing?**
   *
   * The marker (`haltRetried`) is written before the question is asked — by
   * `markScopeRetried` on the resolver's path, and by the expiry branch below for a
   * record this build lifted — precisely so that a worker killed mid-question cannot
   * leave the question unbounded. Its cost is that the marker cannot, by itself, say
   * whether the question was ever *answered*: the write that records the answer is a
   * later, separate `storage.local` call.
   *
   * So the one thing that may not be concluded from it is a **verdict about the
   * account**. A run handed no scope stops, and the record it writes is read by the
   * popup as "the leg could not tell which organization this account uses" — a
   * statement about the account. When the marker says this build's question is still
   * unanswered, this run has heard nothing about the account: it is stopping because
   * of what it was *handed*, not because of anything it observed. Writing that
   * record stamped with this build would convert "never asked" into a judgement, and
   * `haltExpiredBecause` would then keep it for the rest of this build's life — the
   * page never asked again, and the halt asserting a fact nobody established.
   *
   * So such a stop is written **un-stamped** (a later build re-decides it, and asks
   * the page) and the attempt is left unspent (this build does not ask again either).
   * See `halt`'s `verdict` option, and `HaltRetry` in types.ts.
   */
  const attemptUnanswered = (): boolean => haltRetrySpent(currentJudgement());

  /**
   * The same answer, in the shape `halt` takes: `{verdict: false}` when what this
   * run is about to write must not be read as its judgement, and nothing at all when
   * it may be. Spelled once so the two scope-less stops below cannot disagree about
   * which of them is allowed to claim a verdict.
   */
  const unjudged = (): { verdict?: false } => (attemptUnanswered() ? { verdict: false } : {});

  const halt = async (
    reason: HaltReason,
    detail: string,
    opts2: {
      /**
       * 🔴 W59c · `false` = **this run is stopping without having observed anything
       * about the account**, so what it writes is not this build's judgement: no
       * build stamp, and the attempt it was reached under stays unspent.
       *
       * Only the scope-less stops pass it (see `attemptUnanswered`), and it is
       * deliberately not a general escape hatch: every other halt in this file is
       * this run's own observation — a request it sent, a table it read, a record it
       * could not open — and those are exactly the judgements the build stamp exists
       * to name.
       */
      verdict?: false;
    } = {},
  ): Promise<RunReport> => {
    const at = clock.now();
    /**
     * 🔴 W44 · A capability-class record carries what it was a judgement about;
     *    every other reason's record is written **without the field at all** (not
     *    with `undefined`), so the account, upstream and storage records are
     *    byte-identical to what they were before this change.
     */
    const capabilityMark = haltSubjectOf(reason) === 'capability'
      ? { capability: currentCapability() }
      : {};
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
      /**
       * 🔴 W59 · **The permanent record names the build that wrote it.** This is the
       *    statement that makes the next build's re-decision possible at all: a
       *    permanent halt is a judgement one build made, and without this field the
       *    next build cannot tell its own judgement from an inherited one — which is
       *    how the three measured records held two backfillable platforms down.
       *
       * 🔴 Transient records get no stamp (see `HaltRecord.build`), and a build that
       *    cannot name itself writes the field **omitted** rather than empty.
       *
       * 🔴 W59c · And neither does a stop that is not this run's judgement at all
       *    (`verdict: false`): see `attemptUnanswered`.
       */
      const buildMark = currentBuild === null || opts2.verdict === false ? {} : { build: currentBuild };
      state.halted = { reason, at, detail, ...capabilityMark, ...buildMark };
    }
    /**
     * 🔴 W59b · **A halt being written is the verdict on any attempt spent here.**
     *
     * The marker says "this build has already looked and has nothing on disk to show
     * for it". This statement is that something, so the marker is spent and goes — and
     * `redecting` goes with it, because the record just written is the whole answer
     * the run owed and `finish` must not go on to clear it.
     *
     * 🔴 W59c · **Except when the statement is not a verdict** (`verdict: false`).
     *    There the attempt is left exactly as it was found, because what it was
     *    spent on has still not been answered: this build has not asked the page and
     *    must not ask it again, and it must not leave a record that reads as if it
     *    had. Clearing the marker here would hand the next tick a free question — the
     *    per-tick ask W59b removed, one layer out.
     */
    if (opts2.verdict !== false) state.haltRetried = undefined;
    redecting = false;
    await persist(state);
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
    progress: formatProgress(countsOf(state), clock.now()),
    halted: state.halted,
    enumTruncated,
    paceTrace: { enumerate: enumPacer.waits, detail: detailPacer.waits },
    state,
  });

  /**
   * 🔴 W59b · **The run's verdict on a re-decision it suspended, written once.**
   *
   * Every normal exit of this function goes through here rather than straight to
   * `report`, because the re-decision needs an answer **on disk** and the answer is
   * one of two things: a halt (`halt()`, which writes its own record and turns
   * `redecting` off) or "the condition is gone". This is the second one: the record
   * that was suspended is cleared, and the marker that bounded the attempt goes with
   * it, in the same `storage.local` write.
   *
   * 🔴 Why the clear cannot be done at the moment the record expires (which is what
   *    W59 did): between those two points the run does the platform asking, and a
   *    failure anywhere in there — a `storage.local` write, or the service worker
   *    being reclaimed mid-run, which MV3 does routinely — would leave the record
   *    *gone* with nothing written in its place. The next tick would then find no halt,
   *    re-decide again, and ask the platform again, every tick, forever. Suspending
   *    the record instead means the worst case is a leg that stays stopped until the
   *    next build: bounded, and in the direction this project always chooses.
   *
   * 🔴 A write is only made when there is something to clear. A run that never
   *    suspended anything — the ordinary one, and every run whose halt block found no
   *    record — reaches here with `redecting` false and writes nothing.
   *
   * 🔴 W59c · **Both classes reach here owing a verdict, which is why the write is no
   *    longer the capability class's alone.** W59 cleared a capability record at the
   *    expiry itself, so only the other class had something to clear by this point;
   *    W59c suspends both (see the expiry branch), and this is the write that clears
   *    either one — with the marker, in the same `storage.local` call, so the record
   *    and the attempt that bounded it cannot come apart.
   */
  const finish = async (stopped: StopReason): Promise<RunReport> => {
    if (redecting && (state.halted !== null || state.haltRetried !== undefined)) {
      redecting = false;
      state.halted = null;
      state.haltRetried = undefined;
      await persist(state);
    }
    return report(stopped);
  };

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
    /**
     * 🔴 W44/W59 · **A record stops applying when it stops being *this build's*
     *    judgement.**
     *
     * W44 asked that question of the capability class only, and the defect it fixed
     * is the one measured on 2026-09-19: a record saying "this platform has no
     * backfill enumeration yet" is a statement about the build, it stopped being
     * true the moment `PLANS` gained that platform, and the permanence that was
     * supposed to protect it is exactly what made it unable to notice.
     *
     * W59 asks the same question of **every permanent record**, because the same
     * reasoning holds for the rest of them: "the bytes are not a shape we know",
     * "the account has more than one organization and none was named", "our stored
     * record disagrees with itself" are all judgements *a build* made about
     * something it saw, and the build that replaced it has not seen anything. That
     * is what `HaltRecord.build` records and what `haltExpiredBecause` decides.
     *
     * The test is `haltExpiredBecause`, one function shared with the alarm's
     * preflight (`scopeRetryDue`), so no third opinion about "is this record still
     * in force" exists. It compares the record's marker against `currentJudgement()`
     * — the same lookup the halt itself was raised from.
     *
     * What happens when it does not apply:
     *   · the record is **suspended**, and only the record. `pending`, `archived`,
     *     the cursor and every counter are untouched — a capability halt fires
     *     before the request it is about, and for the other permanent reasons the
     *     run's own re-decision is what decides everything from here on;
     *   · the expiry is written down (`state.haltExpired`) **before** the run
     *     continues, because the run may halt again with a different reason and
     *     would then overwrite the only trace that this one ever existed;
     *   · the run carries on into the ordinary path below, where it re-decides:
     *     either the condition is gone — and the leg works — or it is met again and
     *     the same halt is written back, now **stamped with this build**. That is
     *     what makes this one fresh attempt and not a retry: the record the second
     *     run meets names the build that saw the condition, so it applies.
     *
     * 🔴 🔴 W59b · **And it is bounded, which is the half W59 was missing.** The
     *    re-decision's one write used to happen *after* the platform was asked, so a
     *    write that did not land left the older build's record looking untouched and
     *    the next tick asked again — a `GET /api/organizations` per tick for a Claude
     *    scope. So the attempt is written into the header (`haltRetried`) **before**
     *    the run is allowed to ask anything, in this same write, and the record itself
     *    is left exactly as it was found: nothing is claimed on its behalf, and if this
     *    run never gets to write its verdict — a storage failure, an MV3 reclaim — the
     *    record is still there, still says what the older build said, and still stands
     *    for this build. `finish` is what clears it once the run does have an answer.
     *
     *    A record whose attempt is already spent cannot reach this branch at all:
     *    `haltExpiredBecause` answers "still applies" for it (see `haltRetrySpent`),
     *    so the run holds instead of asking. That is what makes this one attempt per
     *    build per platform per scope even when every write in it fails.
     *
     * 🔴 🔴 W59c · **And it is one rule for both classes, not two bounded differently.**
     *    W59b bounded "everything else" this way and left the capability class
     *    exempt, on the grounds that its answer is recomputed from the plan table with
     *    no request at all — so there was nothing to bound and the record was cleared
     *    here, as W59 did. The exemption measured the wrong step: the requests a
     *    capability record is holding back are in the *run that follows*, which is the
     *    leg the record stopped, so a lift whose verdict never landed left `halted:
     *    null` on disk and the platform was asked again on every later tick — the
     *    per-tick question this whole funnel exists to bound, one class over. The
     *    exemption in `haltRetrySpent` is gone with it; what remains of `HaltClass`
     *    here is the transient branch, which the clock (not a build) re-decides.
     *
     *    W44 is untouched by that, and the reason is the marker's *content*: it is
     *    compared against the running build, so a record from any other build is
     *    re-decided exactly as W44 requires. Only the build that already lifted the
     *    record — and has nothing on disk to show for it — holds it.
     */
    const judged = state.halted;
    const expired = haltExpiredBecause(judged, currentJudgement());
    if (haltClassOf(judged.reason) === 'permanent') {
      if (expired === null) {
        if (haltRetrySpent(currentJudgement())) {
          // Held, not re-decided: say so, because the alternative is a leg that looks
          // frozen holding a record another build's name is on.
          console.warn(
            `[chat-stasher] backfill held: the stored ${judged.reason} stop was already`
            + ` re-decided once by this build (${judged.at}) and the verdict could not be`
            + ` written down; the record stands until a new build runs`,
          );
        }
        return finish('halted');
      }

      state.haltExpired = {
        ...expired,
        reason: judged.reason,
        recordedAt: judged.at,
        clearedAt: clock.now(),
      };
      /**
       * 🔴 🔴 W59c · **Both classes suspend the record and spend the attempt, and the
       *    capability class is the reason this is one line rather than two.**
       *
       *    W59 (and W59b for the other class) wrote two different things here: a
       *    capability expiry **deleted** the record and wrote no marker, on the
       *    grounds that the lift costs nothing — the new answer is recomputed from
       *    the plan table in this process. What that missed is the step after it:
       *    deleting the record is what lets this run fetch, and this run's requests
       *    are exactly what the record was holding back. If anything between here and
       *    its verdict goes wrong (a `storage.local` write that throws, an MV3
       *    reclaim of the worker — routine) the disk holds `halted: null`, the next
       *    tick has no record to judge, so it asks the platform again, and the one
       *    after that, forever. Measured shape: the list request the stale record
       *    forbade, re-issued on every alarm wake.
       *
       *    So the record is **suspended** for both classes — left exactly as it was
       *    found, saying what the older build said about a capability or a condition
       *    this build has not yet seen for itself — and the attempt is written into
       *    the header first. `haltRetrySpent` is then what a later tick reads: this
       *    build has already had its one answer here, so the record stands until a
       *    new build. The run that does finish owes a verdict either way — `halt()`
       *    writes the new record and turns `redecting` off, `finish` clears the
       *    suspended one — so nothing is left suspended by a run that succeeds.
       *
       *    🔴 A build that cannot name itself has nothing to spend
       *       (`haltRetrySpent` answers "not spent" for a null build), so no marker is
       *       written for it: that environment keeps the pre-W59c behaviour rather
       *       than gaining a hold nothing can read.
       */
      if (currentBuild !== null) state.haltRetried = { build: currentBuild, at: clock.now() };
      await persist(state);
      redecting = true;
      console.warn(
        expired.because === 'capability'
          ? `[chat-stasher] backfill resuming: the stored ${judged.reason} stop was a judgement about`
            + ` this build's own capability (${expired.judgedAgainst}), which is now`
            + ` ${expired.capability}; the record no longer applies and nothing was written off while it stood`
          : `[chat-stasher] backfill resuming: the stored ${judged.reason} stop was written by`
            + ` ${expired.build}, and this build is ${expired.currentBuild}; a different build's`
            + ` judgement is re-decided once, and if the condition is still there the same stop is`
            + ` written back naming this build`,
      );
    } else {
      // 🔴 A record written before W13 has no `retryAt` and is read as **due now** —
      //    "no delay was ever decided" is not "wait forever". That is precisely the
      //    state the real account was stuck in (reason 'transport-error', detail
      //    starting `list offset=`), and it has to come back on its own.
      const retryAt = state.halted.retryAt;
      if (retryAt !== undefined && clock.now() < retryAt) {
        // 🔴 No request, and no write either: a waiting round must be free. Returning
        //    the persisted record lets the popup say which attempt this is and when
        //    the next one comes — 'waiting-retry' is neither 'ran' nor 'halted'.
        return finish('waiting-retry');
      }

      // Due: the streak continues across the resume, so the ladder does not restart.
      transientStreak = state.halted.attempts ?? 1;
      state.halted = null;
      // 🔴 W59b · The marker outlives the record it was about; with the record gone it
      //    would be an attempt spent on nothing. Cleared with the record it bounded.
      state.haltRetried = undefined;
      await persist(state);
      console.warn(
        `[chat-stasher] backfill resuming after a transient stop`
        + ` (attempt ${transientStreak}; the debts were never touched)`,
      );
    }
  }

  const platformRow = getPlatformByOrigin(opts.origin, opts.channel ?? currentReleaseChannel());
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

  /**
   * 🔴 W31 · **The page size, with the plan's own value in the middle of the chain.**
   *
   * Before this, one number was both the `limit=` a plan asked for and the value a
   * "short page" was measured against, so a plan whose sources name a different
   * page size (claude.ai: 50) could only ever get one of the two right — it would
   * ask for 50 and call 50 rows "short", ending the listing a page early. An
   * explicit `opts.listLimit` still wins: it is a caller's own choice, and the
   * tests that use it are asserting about the engine, not about a platform.
   */
  const listLimit = opts.listLimit ?? plan.listPageSize ?? DEFAULT_LIST_LIMIT;

  /**
   * 🔴 W31 · **A scoped plan with no scope is a named stop, not a request.**
   *
   * The plan's URLs carry `{org}` and `applyScope` refuses to build one without a
   * value, so without this the leg would send a request against a literal
   * `{org}` path. The reason is the resolver's own ('org-unresolved': no
   * organization could be named), it holds **before any request goes out**, and it
   * is emphatically not "you have no conversations" — nothing was enumerated and
   * nothing was written off.
   */
  if (plan.scopeInPath && (typeof opts.scope !== 'string' || opts.scope.length === 0)) {
    return halt(
      'org-unresolved',
      `platform ${plan.platform} addresses conversations by account scope, and this run has none`,
      // 🔴 W59c · Neither of these two stops has heard anything about the account —
      //    they are facts about what this run was handed. See `attemptUnanswered`:
      //    when this build's question is still unanswered on disk, the record must
      //    not claim to be its judgement, and the attempt stays unspent.
      unjudged(),
    );
  }
  /**
   * 🔴 W31 · **'default' is not an organization.**
   *
   * `'default'` is this repository's existing spelling for "the account identifier
   * cannot be told" (entrypoints/background.ts's `identity.value || 'default'`, and
   * the C33 registration that writes it deliberately). For a platform whose *path*
   * carries the scope that word is not a placeholder — it is a string that would be
   * substituted into `/api/organizations/<scope>/chat_conversations` and sent
   * straight at the server. That is the first invariant broken in the most literal
   * way available: an unknown written into a request as if it were a value.
   *
   * So a scoped plan refuses it, with the same reason as "no scope at all", and
   * **before any request**. The cost is written down rather than hidden: a target
   * whose scope has not been resolved yet is stored with this sentinel
   * (entrypoints/background.ts's `UNRESOLVED_SCOPE`), so it ticks, says by name
   * why it cannot proceed, and issues nothing. W31c is what makes that a *passing*
   * state rather than a permanent one: the organization is asked for in the page
   * (lib/backfill/claude-page.ts), at the popup's start button and on a wake-up
   * whose recorded scope is still this word, and a resolved organization replaces
   * the sentinel row. The alternative — substituting the sentinel — would fire a
   * request against an organization that does not exist.
   *
   * 🔴 W49 · A conversation title is the same fact. It is not `'default'`, so the
   *    line above would have let it through and substituted it into the path.
   *    For claude, only an organization id (isClaudeOrgId) may be substituted;
   *    anything else is `org-unresolved`, before any request.
   */
  if (plan.scopeInPath && (
    opts.scope === 'default'
    || (plan.platform === 'claude' && !isClaudeOrgId(opts.scope))
  )) {
    return halt(
      'org-unresolved',
      opts.scope === 'default'
        ? `platform ${plan.platform} addresses conversations by account scope, and 'default'`
          + ' means the identifier could not be told — it is not an organization'
        : `platform ${plan.platform} addresses conversations by account scope, and this run's scope is not an organization`,
      unjudged(),
    );
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
   * 🔴 W21 · **Opaque-token paging** (Grok): the next page needs a cursor the API
   * produced and this code may not interpret. See listTokenUrl in enumerate.ts.
   *
   * 🔴 The engine treats the token as a blob of bytes with one property worth
   *    knowing — whether we have one yet, which is what separates the first page
   *    from every later page (the repeat-page guard below reads exactly that and
   *    nothing more). It is never parsed, compared, sorted, trimmed or logged.
   *    A plan declares one mode or the other; token mode is read first if a plan
   *    ever declared both.
   *
   * 🔴 W22 · The declaration says **where the token goes**, and the engine does
   *    not care: `listTokenUrl` puts it in the query (Grok), `listTokenPost` in
   *    the request body (Kimi). Both are the same paging mode, so both reach the
   *    same branch and the same guard below; only the two lines that build the
   *    request differ.
   */
  // 🔴 W29 · A form body token (`listTokenForm`) is the same paging mode in a
  //    different encoding, so it reaches this branch and the guard below too.
  const tokenMode = plan.listTokenUrl !== undefined
    || plan.listTokenPost !== undefined
    || plan.listTokenForm !== undefined;
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
   *    So a list-only plan gets its **own** cap (`LIST_PAGES_PER_TICK`) rather
   *    than the body-bearing plans' one — a bound it did not have at all until
   *    W59c, which is the change the note below records, and one that no longer
   *    truncates the list because the loop's ending changed with it.
   *
   * 🔴 🔴 W59b · **A re-decision tick reads one page, whatever the plan can do.**
   *    This is the one run in the product where the platform is being asked a
   *    question it was never asked — the halt came off, and the leg finds out what
   *    the wire says now. "A retry must never be a burst" is the reason it is bounded,
   *    and the reason it is bounded *here* rather than by the ordinary budget is that
   *    the ordinary budget for a list-only plan used to be the whole list (74 pages on
   *    the account W10 measured).
   *
   *    🔴 The cap cannot truncate the list, and that took a second change to be true:
   *       stopping the loop early on a list-only plan would reach
   *       halt('detail-unsupported') with the enumeration half-read — and since that
   *       halt is written back stamped with this build, page 2 would never be read at
   *       all. That is precisely the loss the paragraph above forbids. So the
   *       list-only branch *below* returns `budget-exhausted` instead of halting when
   *       the cap is what stopped it (see there).
   *
   * 🔴 🔴 W59c · **And that same ending is what lets the ordinary tick be capped too.**
   *    W59b's cap applied to the re-decision; the tick *after* it was `Infinity` again,
   *    so a stale stop on a 74-page list was one page and then 73 requests in the next
   *    alarm wake (`LIST_PAGES_PER_TICK`'s note has the arithmetic). Nothing about that
   *    ending was specific to a re-decision — "the cap is what stopped this loop" is
   *    the same fact on any tick — so the branch below now asks that question instead
   *    of asking "was this the re-decision". A capped list-only tick ends
   *    `budget-exhausted`, the cursor carries to the next tick, and the halt is reached
   *    once the list really is on disk.
   */
  const listPagesThisTick = redecting
    ? 1
    : canBackfillDetail(plan) ? 1 : LIST_PAGES_PER_TICK;
  let listPagesFetched = 0;
  // The trace has to say which page it stopped on. Under cursor mode `offset` is the
  // **number enumerated so far**, not a request parameter, so the two modes must be
  // worded differently — a trace that reads correctly but points at the wrong thing
  // is worse than none.
  // 🔴 The token's VALUE never reaches a trace: it is an opaque identifier the API
  //    produced, and the log rule here is that identifiers do not go in. Only
  //    "do we have one yet" is printable.
  const listWhere = (): string =>
    tokenMode
      ? `list token=${state.enumCursor.token ? 'set' : 'first-page'} (enumerated ${state.enumCursor.offset})`
      : cursorMode
        ? `list cursor=${state.enumCursor.cursor ?? 'first-page'} (enumerated ${state.enumCursor.offset})`
        : `list offset=${state.enumCursor.offset}`;
  /**
   * 🔴 W21 · **Every id this enumeration has handed us**, for the repeat-page guard.
   *
   * Why a run-local set on top of `state.pending` / `state.archived`, which
   * already hold the ids earlier pages produced: those two are the *persisted*
   * record, and an id can leave them without being settled (`dropDebt` on a
   * failed delivery removes it from pending and does not archive it). This set
   * makes "already seen in this enumeration" true for the whole run regardless of
   * what later happened to the debt, which is the narrower and safer reading.
   */
  const seenThisEnumeration = new Set<string>();
  while (
    !state.enumCursor.complete
    && state.enumCursor.truncated === undefined
    // 🔴 W10: at most `listPagesThisTick` pages here, then the body segment below
    //    gets its turn within the same tick. The bound is on **pages per tick**,
    //    not on the page itself: everything inside this loop is unchanged.
    && listPagesFetched < listPagesThisTick
  ) {
    if (opts.shouldAbort?.()) return finish('aborted');
    await enumPacer.gate();
    anchor('enumerate', enumPacer.lastAt);
    // 🔴 W22 · The token, in one place, for both transports: it reaches the URL
    //    builder or the body builder verbatim and is never inspected. `null` here
    //    means the first page and nothing else.
    const listToken = state.enumCursor.token ?? null;
    const builtListUrl = tokenMode
      ? plan.listTokenUrl
        ? plan.listTokenUrl(opts.origin, listToken, listLimit)
        : plan.listUrl(opts.origin, state.enumCursor.offset, listLimit)
      : cursorMode
        ? plan.listCursorUrl!(opts.origin, state.enumCursor.cursor ?? null, listLimit)
        : plan.listUrl(opts.origin, state.enumCursor.offset, listLimit);
    /**
     * 🔴 W31 · **The one place a scope reaches a request URL.** A plan that
     * declares no `scopeInPath` gets its URL back byte-identical; a scoped plan
     * with no resolvable scope was already stopped above this loop, so a null here
     * would be a bug rather than a state — and it is still turned into a named
     * stop instead of a request against a literal `{org}` path.
     */
    const url = applyScope(plan, builtListUrl, opts.scope);
    if (url === null) {
      return halt('org-unresolved', `${listWhere()}: this plan's URL carries an account scope and none was resolved`);
    }
    // 🔴 C23: the body can only come from the plan's own builder
    //    (listRequestInit / listTokenPostInit → spec.body). No path lets the page
    //    side or the message side decide what is sent here.
    //    🔴 W22: which builder depends on which declaration put this plan in token
    //    mode — a body-cursor plan's body must be able to see the token, and an
    //    offset plan's must not be handed one at all.
    const init = tokenMode && (plan.listTokenPost !== undefined || plan.listTokenForm !== undefined)
      ? listTokenPostInit(plan, opts.origin, listToken, listLimit)
      : listRequestInit(plan, opts.origin, state.enumCursor.offset, listLimit);
    let res: HttpResponse;
    try {
      res = await sendVia(http, url, init);
    } catch (err) {
      const pageReason = (err as Error).message;
      if (pageReason === 'scope-mismatch' || pageReason === 'org-ambiguous' || pageReason === 'org-unresolved') {
        return halt(pageReason, pageReason === 'scope-mismatch'
          ? 'the page active organization differs from the stored backfill scope'
          : `the page could not establish an organization (${pageReason})`);
      }
      return halt('transport-error', `${listWhere()}: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(
        haltReasonForStatus(res.status, plan.platform, res.survivedCredentialReread === true),
        `${listWhere()} returned HTTP ${res.status}`,
      );
    }
    /**
     * 🔴 W61 · **A 2xx is not the same thing as an answer, on a platform that
     * refuses in-band.**
     *
     * Everywhere else in this file a refusal arrives as a status, so the check
     * above is the whole story. DeepSeek answers **HTTP 200** to a request it
     * refuses and puts the failure in the envelope (`code: 40002` /
     * `"Missing Token"`), so without this the body went straight into the parser,
     * its `data` was `null`, and the leg halted **`shape-changed`** — the user was
     * told the API had changed when the platform had said, in as many words, that
     * no token was sent.
     *
     * 🔴 It is asked **before** the parser and before any shape judgement, and it
     *    can only ever replace one halt with a more honest halt: a plan that does
     *    not declare `refusalOf`, and a body that names no refusal, take exactly
     *    the path they took before. Nothing below this line is relaxed.
     */
    const listRefused = plan.refusalOf?.(res.text);
    if (listRefused) {
      return halt(listRefused.reason, `${listWhere()}: ${listRefused.detail}`);
    }
    const parsed = plan.parseListPage(res.text);
    if (!parsed.ok) {
      return halt('shape-changed', `${listWhere()}: ${parsed.detail}`);
    }
    enumeratedPages += 1;
    listPagesFetched += 1;

    /**
     * 🔴 W21 · **The repeat-page guard** (opaque-token paging only).
     *
     * The open question this answers, and why it cannot be answered statically:
     * the sources disagree about Grok's list cursor — two hand back a `pageToken`
     * and one sends an integer `page` — so one of the two shapes is
     * non-functional against the real backend. If the backend ignores the
     * parameter we send, the "second" page is the first page again, and a leg
     * that treated "a page came back" as progress would re-enumerate the same
     * conversations on every tick forever while the ledger said it was advancing.
     *
     * The symptom is decidable without knowing which shape is right: **on a
     * non-first page, a page whose ids this enumeration has already seen means the
     * cursor did not move.**
     *
     * 🔴 It is halt('shape-changed') — a permanent, traced stop — and deliberately
     *    neither of the two things it resembles:
     *     · not `complete = true`: "the cursor did not move" is not "we listed
     *       everything", and writing it as an ending would silently truncate the
     *       account to one page;
     *     · not a silent `break`: the user would be told the backfill finished.
     *    The one thing it does share with a genuine ending is that it stops
     *    hammering the platform — the halt record holds the leg until a human
     *    looks, which is the correct outcome for a wire shape we cannot drive.
     *
     * 🔴 The guard runs **before** `enqueueDebts` below, so a repeated page adds
     *    nothing: its ids are already in pending/archived by construction (that is
     *    the premise of the check), so nothing can be lost by stopping here.
     *
     * 🔴 W31 · **The same question, asked of an offset-paged plan**
     *    (`listOffsetInferred`: claude.ai). There is no token to hold; the
     *    parameter that must move is the `offset` this plan's own URL builder
     *    emits, and "a non-first page carried only ids we have already seen" means
     *    exactly the same thing it does above — the parameter was ignored, and the
     *    next tick would read this page again. The condition is `offset > 0`
     *    rather than "we hold a token", and everything else, including the halt
     *    and its reasoning, is shared: one guard, two ways of knowing a page is
     *    not the first.
     */
    const notFirstPage = tokenMode ? !!state.enumCursor.token : state.enumCursor.offset > 0;
    const guardApplies = tokenMode || plan.listOffsetInferred === true;
    if (guardApplies && notFirstPage && parsed.page.ids.length > 0) {
      const known = new Set<string>([
        ...state.archived,
        ...state.pending,
        ...seenThisEnumeration,
      ]);
      if (parsed.page.ids.every((id) => known.has(id))) {
        return halt(
          'shape-changed',
          `${listWhere()}: the page carried only conversations this enumeration has already seen;`
          + (tokenMode
            ? ' the page cursor did not advance (the platform may not honour the cursor parameter)'
            : ' the page offset did not advance (the platform may not honour the offset parameter)'),
        );
      }
    }
    if (guardApplies) for (const id of parsed.page.ids) seenThisEnumeration.add(id);

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
     *    source — the only public declaration of it, in a reference implementation,
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
    } else if (tokenMode) {
      /**
       * 🔴 W21 · Termination for opaque-token paging.
       *
       * **The token's absence is the API's own end-of-list signal**, and that is
       * not an inference: every source treats a missing/empty `nextPageToken` as
       * the last page, and one of them measured a second page with no overlap
       * before saying so. So this is allowed to set `complete = true`, unlike
       * Perplexity's short/empty-page inference above — there is a field saying
       * it, and the field is what is read.
       *
       * The other ending (an empty page) is handled by the branch above, which
       * already sets `complete = true` for every non-Perplexity platform.
       *
       * 🔴 A token that is present but unusable never reaches here as "absent":
       *    parseGrokListPage returns `{ok:false}` for a non-string
       *    `nextPageToken` (⇒ halt('shape-changed')) and `null` only when the
       *    field is genuinely missing or an empty string. "The field changed
       *    type" and "the API said there is no next page" must not be the same
       *    outcome, or a wire change would read as a finished account.
       */
      const token = parsed.page.nextToken;
      if (token === null || token === undefined) {
        state.enumCursor.complete = true;
      } else {
        state.enumCursor.token = token;
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
    } else if (
      parsed.page.ids.length < listLimit
      && (plan.platform === 'perplexity' || plan.listOffsetInferred === true)
    ) {
      // 🔴 C27 · This is the only "a short page means stop" branch. It is a client
      // inference all three sources share, not a termination signal supplied by the
      // API, so complete must not be set to true.
      // If it is ever confirmed that Perplexity returns a termination field, change
      // this branch: read that field, and set complete=true only when it is
      // explicitly false.
      //
      // 🔴 W31 · claude.ai joins this branch by declaration (`listOffsetInferred`),
      //    and the reason is the same one: its list response is a bare array with no
      //    has_more, no next_cursor and no next-page token — a short page is an
      //    inference, so it is recorded as one and `complete` stays false. The
      //    comparison uses `listLimit`, which for that plan is its own page size
      //    (50, from the plan) rather than the cross-platform default.
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
     * signal all three reviewed reference implementations share.
     *
     * The price, written down rather than hidden: a full enumeration now costs
     * **one extra request** (the empty page at the end). What it buys is that
     * "the list is finished" is never said on the strength of a number this
     * endpoint has already printed wrongly.
     */
    await persist(state);
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
    if (state.pending.length === 0) return finish('queue-empty');
    /**
     * 🔴 W59b / W59c · **A list-only plan must not be halted while its list is still
     *    being read**, and the page cap above is what can put it there.
     *
     * The halt below is a statement about this build's **body segment**. It is the
     * right answer once the enumeration has finished, and a wrong one one page in: it
     * is written back stamped with this build, this build's capability matches
     * `list-only`, and so it applies from the next tick on — the list would stop at
     * page one, permanently, with the debts of every later page never even recorded.
     * That is the same loss `listPagesThisTick`'s note above forbids, so the two
     * changes are one change.
     *
     * So when the *cap* is what ended the loop — not the list ending, and not one of
     * the named truncations the loop records — the tick ends gently instead. Nothing
     * is claimed: `budget-exhausted` says this run's budget ran out, which is exactly
     * what happened, and the next tick reads the rest and reaches this same stop with
     * the whole list on disk.
     *
     * 🔴 W59c · **The condition is the cap, not the re-decision.** W59b wrote it as
     *    `redecting && …`, because the re-decision was the only capped tick then. Now
     *    that every tick of this plan is capped at `LIST_PAGES_PER_TICK`, `redecting`
     *    is not part of the fact: "the loop ended with pages still to read" is, and it
     *    is the same fact on an ordinary tick. Keeping the old condition would have
     *    made a capped ordinary tick reach the halt below with page 1 of 74 on disk —
     *    the truncation this paragraph exists to forbid, reintroduced by the cap that
     *    was supposed to remove only the burst.
     *
     *    Nothing is lost in the ending's own direction: `finish('budget-exhausted')`
     *    does not write a halt, so the plan stays halt-free until its list is really
     *    read out, and `enumCursor.complete`/`truncated` are untouched (they are set
     *    only inside the loop, by a page that ends the list or by a named truncation).
     */
    const cappedByPageCap = !state.enumCursor.complete
      && state.enumCursor.truncated === undefined;
    if (cappedByPageCap) return finish('budget-exhausted');
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
    await persist(state);
  }
  const planCap = pace.detail.maxPerDay;
  const dailyCap = planCap === null
    ? null
    : Math.min(state.detailToday.cap ?? planCap, planCap);
  const budget = opts.maxDetails ?? Number.POSITIVE_INFINITY;

  /**
   * 🔴 W92d · **The empty-body streak is persisted, not run-local.**
   *
   * W92b kept it in a `let` for the life of one `runBackfill`, and R92b §1 measured
   * the loss: an interleaved transient halt (transport-error / rate-limited /
   * daily-cap / `shouldAbort`), or a per-conversation outcome between empties, or a
   * next-build re-decision landing before the K-th empty, discards the count — so an
   * endpoint answering empty for **every** conversation is never recognised and the
   * whole queue is written off `detail-empty` one at a time. The count now lives on
   * the platform-scope state (`state.emptyStreak`), is written by the same `persist`
   * the empty branch already calls, and survives runs, ticks, worker restarts and
   * re-decisions.
   *
   * Incremented **only** on `detail-empty-unverified`; reset to 0 **only** when a
   * body is settled as real content (`settleParkedEmpties`). `detail-tree-incomplete`
   * and `detail-paged-unsupported` are per-conversation facts that used to zero the
   * counter (R92b §1's second split) and no longer do. A legacy state without the
   * field reads as 0 (`stateFrom`), so no version bump and no progress invalidated.
   */
  const bumpEmptyStreak = (): number => (state.emptyStreak = (state.emptyStreak ?? 0) + 1);

  while (state.pending.length > 0) {
    if (opts.shouldAbort?.()) return finish('aborted');
    if (archivedThisRun.length >= budget) return finish('budget-exhausted');
    if (dailyCap !== null && state.detailToday.count >= dailyCap) return finish('daily-cap');

    const id = nextDebt(state);
    if (id === null) break;

    /**
     * 🔴 W92d · **A parked empty id is never fetched twice before proof.**
     *
     * The id reached the head again (either it was parked earlier in this run and
     * the FIFO has come back round to it, or it was parked by a previous run and the
     * debt store restored the original order). Two outcomes, both issuing **no
     * request**:
     *
     *  · there is at least one un-parked id behind it ⇒ rotate it to the tail and go
     *    on, so the proof a real body would give is what gets looked for first;
     *  · every remaining pending id is parked ⇒ stop with the named non-halting
     *    `detail-empty-parked`. There is nothing this run can do: the only thing that
     *    settles these ids is a real body, and there is none left to fetch.
     *
     * It is placed before the pacer gate and before the pre-fetch `persist` so that a
     * skip is genuinely "no request" — the review's objection was that a parked id
     * fetched again would be counted (and paced) as fresh work.
     */
    if (isParkedEmpty(state, id)) {
      const parked = new Set(state.parkedEmpty ?? []);
      if (state.pending.every((pendingId) => parked.has(pendingId))) {
        return finish('detail-empty-parked');
      }
      rotatePendingToTail(state, id);
      continue;
    }

    await detailPacer.gate();
    // 🔴 The moment is persisted **before the request goes out**. Body-fetching is
    // the segment that really has to be gentle: if the SW is reclaimed mid-fetch, or
    // the user closes the browser, the next tick still has to know "one was just
    // fetched", otherwise a restart becomes a back door around the interval.
    // The cost is one extra storage.local write per debt (about every 20 seconds),
    // which is negligible.
    anchor('detail', detailPacer.lastAt);
    await persist(state);
    /**
     * 🔴 W31 · The scope reaches the body URL exactly as it reaches the list URL:
     * through `applyScope`, in one place, so a scoped plan's two segments cannot
     * end up addressing different organizations. The named stop below is the same
     * one the list loop has — a scoped plan with no scope never reaches this loop
     * in production, and if it ever did it must not send a literal `{org}` path.
     */
    const scopedDetailUrl = applyScope(plan, detailUrlOf(opts.origin, id), opts.scope);
    if (scopedDetailUrl === null) {
      return halt('org-unresolved', 'detail: this plan\'s URL carries an account scope and none was resolved');
    }
    const url = scopedDetailUrl;
    const init = detailRequestInit(plan, opts.origin, id);
    let res: HttpResponse;
    try {
      res = await sendVia(http, url, init);
    } catch (err) {
      const pageReason = (err as Error).message;
      if (pageReason === 'scope-mismatch' || pageReason === 'org-ambiguous' || pageReason === 'org-unresolved') {
        return halt(pageReason, pageReason === 'scope-mismatch'
          ? 'the page active organization differs from the stored backfill scope'
          : `the page could not establish an organization (${pageReason})`);
      }
      // 🔴 A POST failure and a GET failure take **the same line**:
      //    halt('transport-error') + a persisted trace. No silent path was opened
      //    for POST.
      return halt('transport-error', `detail: ${(err as Error).message}`);
    }
    if (res.status < 200 || res.status > 299) {
      return halt(haltReasonForStatus(res.status, plan.platform, res.survivedCredentialReread === true), `detail returned HTTP ${res.status}`);
    }

    /**
     * 🔴 W21 · **The optional second step** (a plan may declare `detailStep2`).
     *
     * Some platforms split one conversation across two same-origin calls: a
     * skeleton call that names the message ids, then a content call that takes
     * those ids in its request body. Everything below is generic on purpose — the
     * engine knows "there is a second URL and a body built from step 1's own
     * response", and nothing else. It never sees the ids, never learns what an id
     * is, and never builds a body: `step2.body()` is the plan's own builder, the
     * same rule the first step's POST body already follows.
     *
     * 🔴 What is **delivered** is step 2's response and only step 2's. Step 1's
     *    body is not archived anywhere: it is structure, not conversation, and
     *    storing it would put a second, contentless copy of every conversation in
     *    the archive.
     * 🔴 Pacing and the daily cap count the **pair as one body**: the detail
     *    pacer's gate already fired once above, and `detailToday.count` increments
     *    once below. The wait inside the pair is `delayMs`, drawn per conversation
     *    from the run's injected randomness — an intra-pair gap, not an
     *    inter-request rate, so it is deliberately not a Pacer.
     * 🔴 `body()` returning null means step 1 could not be read (or named nothing
     *    to fetch). Nothing is sent, nothing is settled, and it halts as
     *    'shape-changed': "the skeleton did not give us a request we can build" is
     *    a wire-shape fact, and the alternative — sending `{ ids: [] }` and
     *    calling the empty answer a conversation — is the exact confusion this
     *    repository's first invariant forbids.
     */
    const step2 = plan.detailStep2;
    let deliveredUrl = url;
    let deliveredMethod: string = init.method;
    let deliveredStatus = res.status;
    let deliveredText = res.text;
    if (step2) {
      const body = step2.body(id, res.text);
      if (body === null) {
        return halt(
          'shape-changed',
          'detail step 1 named no message ids this plan can read; nothing was sent for step 2',
        );
      }
      await clock.sleep(uniformBetween(random, step2.delayMs.min, step2.delayMs.max));
      const step2Url = step2.url(opts.origin, id);
      let res2: HttpResponse;
      try {
        res2 = await sendVia(http, step2Url, { method: 'POST', body, contentType: step2.contentType });
      } catch (err) {
        return halt('transport-error', `detail step 2: ${(err as Error).message}`);
      }
      if (res2.status < 200 || res2.status > 299) {
        return halt(haltReasonForStatus(res2.status, plan.platform, res2.survivedCredentialReread === true), `detail step 2 returned HTTP ${res2.status}`);
      }
      deliveredUrl = step2Url;
      deliveredMethod = 'POST';
      deliveredStatus = res2.status;
      deliveredText = res2.text;
    }

    /**
     * 🔴 W29 · **A body that is more than one request** (a plan may declare
     * `detailPages`).
     *
     * The engine's part is exactly what `DetailPagesSpec` says it knows: a URL, a
     * request built for a later page from a token this loop never reads, a wait, a
     * cap, and an assembler. It never learns what the token is, never parses it,
     * never compares it, and never builds a body. Two rules are worth stating:
     *
     * 🔴 **The whole loop is one body.** The pacer's gate fired once above and
     *    `detailToday.count` increments once below, so a twenty-page conversation
     *    costs one slot — the unit the product cares about is "a conversation
     *    fetched", not "an HTTP request sent". The wait between pages is
     *    `delayMs`, drawn per page, and it is deliberately not a Pacer.
     * 🔴 **The cap is a refusal, not a truncation.** Reaching `maxPages` with a
     *    token still in hand means this conversation is longer than this leg will
     *    fetch: the pages collected so far are real content and still not the
     *    conversation, so nothing is stored, the debt leaves pending, and the
     *    failure list carries `detail-too-long`. The alternative — storing what
     *    was collected and calling it complete — is the one outcome this whole
     *    design exists to prevent.
     * 🔴 **A page whose turns are all already seen is the cursor not advancing**
     *    (a platform that ignored the token would hand page 1 back forever, and
     *    the ledger would say the conversation was progressing). Same rule and
     *    same halt as the list segment's repeat-page guard. Partial overlap is
     *    tolerated: the probe measured disjoint pages, the sources expect
     *    overlap, and only "nothing new at all" is decidable without knowing
     *    which of the two is right.
     */
    if (!step2 && plan.detailPages) {
      const pages = plan.detailPages;
      const rawPages: string[] = [res.text];
      const seenResponseIds = new Set<string>();
      let step = pages.nextPage(res.text, id);
      let tooLong = false;
      for (;;) {
        if (step.kind === 'unreadable') {
          return halt('shape-changed', `detail page: ${step.reason}`);
        }
        if (step.kind !== 'more') break;
        if (
          step.responseIds.length > 0
          && step.responseIds.every((responseId) => seenResponseIds.has(responseId))
        ) {
          return halt(
            'shape-changed',
            'detail page: a page carried only turns this conversation has already returned;'
            + ' the page cursor did not advance',
          );
        }
        for (const responseId of step.responseIds) seenResponseIds.add(responseId);
        if (rawPages.length >= pages.maxPages) {
          tooLong = true;
          break;
        }
        if (opts.shouldAbort?.()) return finish('aborted');
        await clock.sleep(uniformBetween(random, pages.delayMs.min, pages.delayMs.max));
        const pageUrl = pages.url(opts.origin, id);
        const pageInit = pages.nextInit(opts.origin, id, step.token);
        let next: HttpResponse;
        try {
          next = await sendVia(http, pageUrl, pageInit);
        } catch (err) {
          return halt('transport-error', `detail page: ${(err as Error).message}`);
        }
        if (next.status < 200 || next.status > 299) {
          return halt(haltReasonForStatus(next.status, plan.platform, next.survivedCredentialReread === true), `detail page returned HTTP ${next.status}`);
        }
        if (!matchesResponseShape(platformRow, next.text)) {
          return halt('shape-changed', `detail page does not match the ${platformRow.id} response shape`);
        }
        rawPages.push(next.text);
        step = pages.nextPage(next.text, id);
      }
      if (tooLong) {
        // 🔴 W92d · This `continue` is above the empty-body branch, so it is also
        //    above the line that used to zero the streak. It must **not** zero it:
        //    "this one conversation is longer than this leg fetches" says nothing
        //    about an endpoint that is answering empty for everything, and R92b §1
        //    named `detail-too-long` as one of the outcomes the old run-local counter
        //    was wrongly reset by. The debt leaves pending with its own receipt; the
        //    empty streak and the parked empties are untouched.
        dropDebt(state, id);
        failedThisRun.push(
          recordFailure(state, { id, reason: 'detail-too-long', at: clock.now() }),
        );
        state.detailToday.count += 1;
        await persist(state);
        console.warn(
          '[chat-stasher] backfill: this conversation needs more pages than this leg fetches'
          + ' in one body; nothing was stored and the failure list names it',
        );
        continue;
      }
      // The delivered document is the assembled bundle; the recorded request is
      // the **first** page's, i.e. the one the page itself also makes. Every raw
      // page is inside the bundle, so the archive holds the whole exchange.
      deliveredText = pages.assemble(id, rawPages);
    }

    /**
     * 🔴 W61 · **The in-band refusal, asked before the shape gate.**
     *
     * This is the body segment's half of the reason `refusalOf` is a plan field
     * rather than a few lines in the list parser: a refusal envelope has no `data`,
     * so it fails `matchesResponseShape` just below and never reaches
     * `parseDetailPage`. Waiting for the parser to notice would mean the leg
     * reported `shape-changed` for a body that says `INVALID_TOKEN`.
     *
     * It is asked here, on the **delivered** body, for the same reason the shape
     * gate is (see the W21 note above): on a two-step plan this is step 2's
     * response, which is the artefact the platform row describes.
     */
    const detailRefused = plan.refusalOf?.(deliveredText);
    if (detailRefused) {
      return halt(detailRefused.reason, `detail body: ${detailRefused.detail}`);
    }

    // The same shape checker the live leg uses: if the API changes, this is the first to know.
    // 🔴 W21: on a two-step plan this is applied to the DELIVERED body (step 2),
    //    which is the one the platform row describes. Step 1 has no shape gate of
    //    its own because it is not the artefact — an unreadable step 1 already
    //    halted above, through the plan's own body builder.
    if (!matchesResponseShape(platformRow, deliveredText)) {
      return halt('shape-changed', `detail body does not match the ${platformRow.id} response shape`);
    }

    /**
     * 🔴 C28 · This is the single guardrail landing point for a future body parser:
     * after the shape is good and before the sink.
     *
     * 🔴 W21 changed one thing here: the parser is handed the **delivered** body
     *    (`deliveredText`), which on a two-step plan is step 2's response. On a
     *    one-step plan this is byte-for-byte the old `res.text`, so no existing
     *    platform's behaviour moves. Grok is the first production plan to declare
     *    one, and it exists for exactly the case this hook describes.
     *
     * 🔴 W92d · **One empty body is a per-conversation question that is parked, not
     *    answered, and never written off on sight.**
     *
     *    C28's first version halted the whole leg on one empty body and left the id
     *    at the head of pending, so every later run halted on the same conversation
     *    (measured on Claude: an opened-but-never-sent conversation, HTTP 200 with
     *    `chat_messages: []`, stopped the platform from archiving anything — W92
     *    §Task 5). W92b swung the other way and dropped the id immediately, and R92b
     *    measured that as irreversible: nothing re-enqueues it (enumeration is
     *    complete), so a changed endpoint answering empty for **every** conversation
     *    writes the whole queue off, two ids per run.
     *
     *    W92d keeps the intent of both. The plan's answer still goes into the ledger
     *    as a receipt (`complete:false`), but the id is **parked** — moved to the
     *    tail of pending and remembered in `state.parkedEmpty` — and **no**
     *    `detail-empty` failure is written yet. It is dropped with that receipt only
     *    once a later body in the same scope is archived as real content, the proof
     *    the endpoint still answers with conversations. If the user later writes in
     *    an empty conversation, live capture still takes it.
     *
     *    🔴 C28's original intent is kept by a **persisted** streak: a whole endpoint
     *       answering empty is a contract change, and `DETAIL_EMPTY_HALT_STREAK`
     *       consecutive `detail-empty-unverified` bodies — counted across runs,
     *       transient halts and next-build re-decisions — halt the leg with the
     *       reason C28 introduced, leaving every parked id in `pending`. Only a real
     *       body archived resets the streak (see `settleParkedEmpties`).
     *
     * 🔴 W22 added a third answer, handled in its own branch just below:
     *    'detail-paged-unsupported' — real content that the plan knows is
     *    incomplete — which must neither be archived nor halt the leg.
     */
    const detailParsed = plan.parseDetailPage?.(deliveredText);
    if (detailParsed?.ok === false) {
      return halt('shape-changed', `detail body: ${detailParsed.detail}`);
    }
    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-empty-unverified') {
      const entry = recordDetailOutcome(id, detailParsed.outcome, clock.now());
      const streak = bumpEmptyStreak();
      // The request really did go out, whether it leads to the halt or to a park, so
      // it counts against the day's quota before either branch persists.
      state.detailToday.count += 1;
      if (streak >= DETAIL_EMPTY_HALT_STREAK) {
        // 🔴 The K-th id is deliberately **not** parked: it stays at the head of
        //    pending so a later build's re-decision re-fetches it, sees the same
        //    empty body and lets the streak keep accumulating instead of the guard
        //    quietly going silent because every id was already skipped.
        await persist(state);
        return halt(
          'detail-empty-unverified',
          `detail returned HTTP ${deliveredStatus} with empty content for a pending`
          + ` conversation, ${streak} in a row since the last real body (across runs and`
          + ` transient stops); recorded ${entry.outcome} with complete=false`,
        );
      }
      parkEmpty(state, id);
      await persist(state);
      console.warn(
        '[chat-stasher] backfill: this conversation\'s body came back empty'
        + ` (${streak} since the last real body, still below the ${DETAIL_EMPTY_HALT_STREAK}`
        + ' that would look like a contract change); the id is parked — it stays owed, and'
        + ' is written off only once a later real body proves the endpoint still works',
      );
      continue;
    }
    // 🔴 W92d · **Nothing below resets the streak.** A real body resets it where it
    //    is archived (`settleParkedEmpties`); a confirmed-empty body, a too-long
    //    body above, and the two named "incomplete" outcomes below are all
    //    per-conversation facts and must not launder an endpoint that is answering
    //    empty for everything back into a clean slate (R92b §1's second split).
    /**
     * 🔴 W22 · **A real body that is explicitly incomplete.**
     *
     * The plan recognised the response and it says there is more of this
     * conversation than it carries (Kimi: a non-empty next-page token). This leg
     * does not page that endpoint and will not invent paging, so the one thing
     * that must not happen is what the shape gate would otherwise allow: the body
     * is stored and the debt settled, i.e. a **truncated conversation archived as
     * a complete one** — the archive would then hold a partial answer to "what did
     * I say" and nothing would say so.
     *
     * So it takes the failure path, exactly as C20 defined it for a body that
     * could not be stored: the debt leaves pending (no retry — a product decision,
     * see lib/backfill/failures.ts), a person-readable receipt with this reason
     * code and the conversation's short id goes on the failure list, and the leg
     * carries on with the next conversation. Nothing enters `archived`.
     *
     * 🔴 Why the loop continues instead of halting: this is a **per-conversation**
     *    fact, not a wire change. A long conversation is long; the short ones
     *    beside it are complete and must still be archived in this same run. The
     *    count below is deliberate too — the request really did go out, so it
     *    counts against the day's quota, the same as a failed store does.
     */
    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-paged-unsupported') {
      dropDebt(state, id);
      failedThisRun.push(
        recordFailure(state, { id, reason: 'detail-paged-unsupported', at: clock.now() }),
      );
      state.detailToday.count += 1;
      await persist(state);
      console.warn(
        '[chat-stasher] backfill: this conversation\'s body says it is incomplete'
        + ' (the platform offers more than one response holds, and this leg does not page it);'
        + ' nothing was stored and the failure list names it',
      );
      continue;
    }
    /**
     * 🔴 W84b · **A real body that does not prove it is the whole conversation.**
     *
     * Perplexity alone answers here: the response is recognised and carries real
     * content, but its completeness signal is not the exact confirmed-no-more pair
     * (`has_next_page === false` + `next_cursor === null`) — one key missing,
     * `next_cursor: ""`, a wrong-typed key — or an entry holds no readable content
     * (an empty `blocks` and no non-empty `text`). Either way the archive would be
     * settling debt on a body that never said, in the form this code reads, that
     * it is complete.
     *
     * A per-conversation fact, exactly like 'detail-paged-unsupported': every
     * other conversation in the same run is unaffected. The debt leaves pending
     * (no retry), a person-readable receipt with this reason and the
     * conversation's short id goes on the failure list, and the leg carries on
     * with the next conversation. Nothing enters `archived`.
     */
    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-unverified') {
      dropDebt(state, id);
      failedThisRun.push(
        recordFailure(state, { id, reason: 'detail-unverified', at: clock.now() }),
      );
      state.detailToday.count += 1;
      await persist(state);
      console.warn(
        '[chat-stasher] backfill: this conversation\'s body does not prove it is whole'
        + ' (its no-more signal is absent, empty or wrong-typed, or an entry carried no readable content),'
        + ' so nothing was stored; the failure list names it',
      );
      continue;
    }
    /**
     * 🔴 W31 · **A recognised body whose own parent links cannot be walked.**
     *
     * Both platforms that reach this are trees with a named current leaf, and the
     * fact recorded is that the branch could not be resolved — not why. For claude.ai
     * the walk ends at an absent parent as the branch root (🔴 W92: the real wire's
     * branch root names a shared sentinel no body carries), so it reaches this only
     * for a missing leaf or a cycle; for DeepSeek an absent parent still counts.
     *
     * This is the same shape of decision as the branch above — a per-conversation
     * fact, a named receipt, nothing archived, and the run carries on — and it is
     * deliberately the *same path* rather than a second mechanism with its own
     * rules: the debt leaves pending (no retry), `detail-tree-incomplete` goes on
     * the failure list with the conversation's short id, the request that really
     * went out counts against the day's quota, and the leg moves to the next
     * conversation. Archiving it instead would put a partial answer to "what did I
     * say" in the archive with nothing marking it partial — the one outcome the
     * whole design exists to prevent.
     */
    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-tree-incomplete') {
      dropDebt(state, id);
      failedThisRun.push(
        recordFailure(state, { id, reason: 'detail-tree-incomplete', at: clock.now() }),
      );
      state.detailToday.count += 1;
      await persist(state);
      console.warn(
        '[chat-stasher] backfill: this conversation\'s branch could not be walked'
        + ' (the current leaf is missing, or its parent links do not form a readable chain);'
        + ' nothing was stored and the failure list names it',
      );
      continue;
    }

    if (detailParsed?.ok === true && detailParsed.outcome === 'detail-empty-confirmed') {
      recordDetailOutcome(id, detailParsed.outcome, clock.now());
      // A legitimately empty conversation is not a loss: it may be settled, but its
      // trace must be separate from the unverified empty above.
      settleDebt(state, id);
      archivedThisRun.push(id);
      state.detailToday.count += 1;
      await persist(state);
      continue;
    }

    const captured: CapturedFetch = {
      // 🔴 W21: the URL and method of the request that actually produced this
      //    body. On a one-step plan that is step 1's, byte-for-byte as before; on
      //    a two-step plan it is step 2's, which is the one that carries the
      //    content and therefore the honest record of where it came from.
      url: deliveredUrl,
      // 🔴 C23: write the method actually sent, faithfully. A GET segment is still
      // byte-for-byte 'GET' (init.method defaults to it).
      method: deliveredMethod,
      status: deliveredStatus,
      text: deliveredText,
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
      await persist(state);
      console.warn(`[chat-stasher] backfill paused: ${verdict.reason} — ${verdict.detail}`);
      return finish('host-unavailable');
    }

    if (verdict.ok) {
      settleDebt(state, id);
      archivedThisRun.push(id);
      // 🔴 W92d · **A real body archived is proof the endpoint works, and only that
      //    settles the parked empties.** The scope has just answered with a
      //    conversation, so "these earlier bodies really were empty" is now
      //    supportable: the parked ids leave pending with a `detail-empty` receipt
      //    and the streak goes back to 0. Any other outcome leaves both untouched.
      settleParkedEmpties(clock.now());
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
    await persist(state);
  }

  return finish('queue-empty');
}
