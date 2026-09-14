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

// 🔴 W16 · The one source of randomness, injected. `types.ts` still does no I/O
//    of its own — this only pulls in a pure function and the production draw.
import { systemRandom, uniformBetween, type RandomFn } from './random';

/**
 * 🔴 W18 · The **persisted layout** version, not the shape of the in-memory state.
 *
 * It went 1 → 2 because the state stopped being one record. A v1 record is a whole
 * `BackfillState` (debt ids included) at one `storage.local` key; a v2 record is a
 * small header at `cs_backfill_v2:<platform>:<scope>` plus the debt ids in
 * IndexedDB (`lib/backfill/debt-store.ts`). This constant drives `stateKey()`, so
 * the storage key moved with it and the popup's `STATE_KEY_PREFIX` — which is
 * computed from the same constant — followed without a second edit.
 *
 * The old key has a name of its own (`legacyStateKey`) and is read, once, by the
 * migration in `lib/backfill/ledger.ts`.
 */
export const BACKFILL_STATE_VERSION = 2;

/**
 * The layout version this repository wrote before W18 — one record holding
 * everything, at `cs_backfill_v1:<platform>:<scope>`.
 *
 * 🔴 It is a literal and not `BACKFILL_STATE_VERSION - 1`: it names a specific
 *    layout that existed, and the migration has to keep recognising exactly that
 *    one however many times the current version is bumped.
 */
export const LEGACY_STATE_VERSION = 1;

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
  | 'detail-empty-unverified'
  /**
   * 🔴 W18 · **A state record is there, and it is not something we can read.**
   *
   * This is a different fact from 'storage-unavailable' and must not borrow its
   * wording: there the store is missing, here the store answered and what came
   * back does not parse as either layout.
   *
   * The leg then writes **nothing at all** — not a debt record, not the header —
   * and leaves the unreadable record exactly where it found it. Turning "we could
   * not read your progress" into "you have no progress" is the single mistake this
   * project exists to avoid (CLAUDE.md invariant 1), and it is one `?? []` away.
   *
   * Permanent: no amount of waiting makes a malformed record parse; a human has to
   * look at it.
   */
  | 'state-unreadable';

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

/**
 * 🔴 W13 · **Which kind of stop this is** — the distinction that did not exist, and
 * whose absence froze a real account's backfill for over an hour.
 *
 * Before this, every `HaltReason` meant the same thing to the engine: write
 * `state.halted` and refuse to run until a human clears it. Measured on a real
 * Chrome profile: one round at 09:14 ended in `transport-error` because a page
 * reload tore the extension's message channel; every round after it returned
 * `halted` immediately, and 7,391 debts stayed untouched for more than an hour
 * with nothing wrong with the account, the login, or the network.
 *
 * So the question a stored halt has to answer is not only "why did it stop" but
 * **"is there anything to wait for"**:
 *   · 'transient'  — the platform/transport said "not right now". The same
 *                    request is the right thing to send again, later. The leg
 *                    resumes **on its own** once the backoff expires.
 *   · 'permanent'  — no amount of waiting changes the answer: the response shape
 *                    is not one we know, or the code cannot do this platform yet.
 *                    A human has to look, and this is the old semantics verbatim.
 *
 * 🔴 This is the "unknown must never be recorded as empty" invariant applied to
 *    time: "we do not know yet, and we will ask again at T" is a different fact
 *    from "we will not get this without a human", and collapsing them into one
 *    `halted` is what made a one-second glitch permanent.
 */
export type HaltClass = 'transient' | 'permanent';

/**
 * Classify one reason. Pure, no I/O, so engine / progress / popup-view all ask the
 * same function instead of each keeping its own list — the failure mode being
 * avoided is two lists that disagree about `rate-limited`.
 *
 * transient:
 *  · 'transport-error' — the transport threw, so nothing about the account was
 *    observed; the same request is exactly right to send again, and the case that
 *    really happened (a reload tearing the message channel) heals in seconds.
 *  · 'rate-limited'    — 429/403/5xx is the platform saying "not now". A reference
 *    implementation treats this one as retryable *and differently from other errors* (longer
 *    base, hard ceiling) — see the retry notes below.
 *
 * permanent:
 *  · 'shape-changed'          — the bytes are not a shape we recognise; sending
 *    the identical request returns the identical unrecognised bytes. Waiting is
 *    not a remedy; a human reads the shape.
 *  · 'unsupported-platform' / 'detail-unsupported' — the code cannot do this yet.
 *    Both fire *before any request goes out*, and both reasons' own doc comments
 *    above say they mean "wait for a fix".
 *  · 'storage-unavailable'    — not a platform condition, and structurally
 *    un-retryable: with no store there is nowhere to persist a retry moment, so
 *    an automatic retry would re-decide the same thing on every tick forever.
 *  · 'detail-empty-unverified' — C28. The body endpoint answered and the shape was
 *    fine, but this conversation came back empty and that must **not** be turned
 *    into "it really is empty". Retrying it automatically would convert C28's "we
 *    do not know" into an endless request loop against one conversation (nothing
 *    behind it can progress while it re-fails), and a body that suddenly parses to
 *    nothing is most likely a contract change — the human-look class. The C28
 *    receipt stays the trace.
 */
export function haltClassOf(reason: HaltReason): HaltClass {
  return reason === 'transport-error' || reason === 'rate-limited' ? 'transient' : 'permanent';
}

/**
 * How long to wait before the next attempt, per reason.
 *
 * 🔴 Shape borrowed from a reference implementation, numbers re-based on our
 *    clock. There, retries are
 *    `baseDelay * multiplier^n`, clamped by `maxDelay`; the defaults are
 *    `baseDelay: 1000 ms, multiplier: 2`, and **once a 429 is seen the ladder is
 *    switched to `baseDelay: 60000 ms, multiplier: 2, maxDelay: 300000 ms`**.
 *    That structure — geometric growth with a ceiling, and a longer ladder for
 *    rate-limiting than for transport errors — is what these numbers keep.
 *
 * What is re-based: the reference implementation's retries happen *inside one session's request loop*,
 * so 1 s is a natural unit there. Ours happens once per alarm tick, and the
 * shortest possible tick gap is `BACKFILL_TICK_DELAY_MIN_MINUTES = 5`
 * (lib/backfill/alarm.ts — the tick is jittered since W16, and this is its
 * floor). Any delay at or below that floor therefore degenerates to "the next
 * tick retries" — a perfectly reasonable outcome, but not a backoff, and
 * shipping it with a tuned looking number would be a lie about what has been
 * tuned. So the unit here is one tick, and the ladders are expressed in ticks:
 *
 *   reason            base     cap      in ticks        why these
 *   ---------------   ------   ------   -------------   -------------------------
 *   transport-error    5 min   30 min   1 → 2 → 4 → 6   one skipped tick is the
 *                                                       gentlest thing that is
 *                                                       still a backoff, and the
 *                                                       failure being fixed (a
 *                                                       torn channel from a page
 *                                                       reload) is gone by then.
 *                                                       30 min caps a genuinely
 *                                                       dead channel at 2
 *                                                       requests/hour, not 12.
 *   rate-limited      15 min   60 min   3 → 6 → 12      a rate limit is not one
 *                                                       bad tick, so it starts a
 *                                                       whole ladder higher; the
 *                                                       60 min ceiling is one
 *                                                       request/hour while
 *                                                       limited — a 12x reduction
 *                                                       from the normal rate.
 *
 * 🔴 The reference implementation's ratios are 60x (base) / 5x (cap) on the 429 ladder; ours are 3x / 4x.
 *    Deliberately milder, for one reason: its ladder **gives up** after 2
 *    retries, so it can afford to be aggressive. This leg never writes a debt off
 *    (only a real delivery settles one), so a persistent 429 has to land on a
 *    sustainable steady state rather than a deadline. Re-basing its 300 s cap
 *    literally would be 5 min = exactly one tick = no backoff at all.
 *
 * Monotonic, then flat: `attempts` only ever spaces requests further apart, so
 * this change can never issue *more* requests than the old code — for a permanent
 * halt it issues exactly as many (zero), and for a transient one strictly fewer
 * than a naive "just clear `halted`" fix, which would re-fire on the next tick.
 */
export const TRANSIENT_RETRY_BASE_MS: Record<'transport-error' | 'rate-limited', number> = {
  'transport-error': 5 * 60_000,
  'rate-limited': 15 * 60_000,
};

/** The ceiling of each ladder. Never exceeded, however long the streak runs. */
export const TRANSIENT_RETRY_MAX_MS: Record<'transport-error' | 'rate-limited', number> = {
  'transport-error': 30 * 60_000,
  'rate-limited': 60 * 60_000,
};

/** The transient reasons this ladder is defined for. A permanent reason has no delay at all. */
export type TransientHaltReason = keyof typeof TRANSIENT_RETRY_BASE_MS;

export function isTransientReason(reason: HaltReason): reason is TransientHaltReason {
  return reason === 'transport-error' || reason === 'rate-limited';
}

/**
 * `base * 2^(attempts-1)`, clamped to the cap — **and then jittered**.
 * `attempts` is the consecutive-failure count **including this one**, so
 * attempt 1's exponential value is exactly `base`.
 * The exponent is bounded before the multiply: a long-running streak must not
 * produce `Infinity` on its way to a cap that is 30 minutes.
 *
 * 🔴 W16 · **Full jitter**: the returned delay is
 * `uniform[0.5, 1.0] × (base · 2^(attempts-1))`, still clamped to the cap.
 *
 * Why a backoff needs jitter at all, when it is already exponential: every
 * client that failed at the same instant schedules its retry for the same
 * instant, so a deterministic ladder *re-synchronises* the traffic it was meant
 * to spread out — the platform gets the whole cohort back in one lump, one rung
 * later. Drawing the delay decorrelates them.
 *
 * 🔴 This is the one place in W16 where a jittered draw can be **below** the old
 *    deterministic value (half of it, at the bottom of the band), because that
 *    is what full jitter is. Two things keep that from being a raised request
 *    rate:
 *      · the ceiling is unchanged — the cap still holds, and nothing above it
 *        is reachable;
 *      · the actual request is still gated by the detail pacer's own minimum
 *        interval (20 s), which is three orders of magnitude below the smallest
 *        delay this can return (0.5 × 5 min = 2.5 minutes for a transport
 *        error, 0.5 × 15 min = 7.5 minutes for a 429).
 *    So the retry *schedule* shifts earlier; the request *rate* does not rise.
 *
 * `random` is the last parameter with the production source as its default, so
 * every existing call keeps its exact meaning and a test can pin both ends of
 * the band with `() => 0` (half) and `() => 1` (the full exponential).
 */
export function transientRetryDelayMs(
  reason: TransientHaltReason,
  attempts: number,
  random: RandomFn = systemRandom,
): number {
  const base = TRANSIENT_RETRY_BASE_MS[reason];
  const max = TRANSIENT_RETRY_MAX_MS[reason];
  const step = Math.max(1, Math.floor(attempts)) - 1;
  // 2^40 is already far past both caps; clamping the exponent keeps the product finite.
  const factor = 2 ** Math.min(step, 40);
  const exponential = base * factor;
  return Math.min(uniformBetween(random, 0.5, 1) * exponential, max);
}

export interface HaltRecord {
  reason: HaltReason;
  /** When it happened (clock.now(), milliseconds) */
  at: number;
  /** Technical detail only: URL path, status code, which field is missing. Never a conversation body. */
  detail: string;
  /**
   * 🔴 W13 · **Transient records only**: the earliest moment a new run may try this
   * platform again (`clock.now()` ms). `undefined` on a permanent record — and on a
   * record written before W13, where it is read as "no delay was ever decided", i.e.
   * **due now** (see the resume path in engine.ts).
   *
   * Why it lives inside the halt record rather than in a storage key of its own:
   * the popup's question is "why is this leg stopped", and a retry moment in a
   * parallel key could disagree with `halted` about whether the leg is stopped at
   * all. One record, one answer — and it is what makes the legacy case decidable
   * with no new key: a stored record without `retryAt` is either permanent (stop)
   * or transient-and-due (retry).
   */
  retryAt?: number;
  /**
   * 🔴 W13 · Transient records only: how many **consecutive** transient failures led
   * to this record (>= 1). It is the exponent of the backoff ladder and the number
   * the popup shows as "attempt N", so it has to survive restarts: the only carrier
   * is this record, and a run that ends without a transient halt resets the streak
   * simply by not writing one.
   */
  attempts?: number;
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
  /**
   * 🔴 W13 · This leg hit a **transient** condition (transport error / rate limit)
   * and is now waiting out its backoff. It will carry on by itself, from the same
   * cursor and the same debt set, once `halted.retryAt` passes.
   *
   * Why it must be a value of its own rather than reusing either neighbour:
   *   · not `halted` — `halted` means "a human has to look", and that is precisely
   *     the reading that turned one torn channel into an hour of a frozen leg;
   *   · not `queue-empty` — nothing is finished, 7,391 debts were still owed in the
   *     measured case, and this reason must never be mistaken for "nothing left";
   *   · not `ran` — no request went out during a waiting round.
   * Its difference from 'host-unavailable': that one is the *delivery exit* being
   * absent while this leg itself is healthy (and it retries on `hello`); this one is
   * the *platform* having just refused or dropped a request.
   */
  | 'waiting-retry'
  | 'halted';

/**
 * Where `total` came from. Only 'response-total' is fit to be the progress bar's
 * denominator.
 *
 * 🔴 W10 · 'contradicted' — **the API's own total was disproved by what the list
 *    actually returned.** A real account was measured with `total = 901` while
 *    7,391 distinct conversation ids came back from the very same endpoint, so
 *    `total` is not "how many conversations this account has"; it is at best a
 *    number the endpoint happens to print.
 *
 *    The value is set by measurement only: after a page is read, if the rows
 *    listed so far outnumber the total the API reported, the total is disproved
 *    and never becomes a denominator again. The measurement is the evidence —
 *    nothing here reads a field whose meaning is unknown (see the note in
 *    lib/backfill/enumerate.ts on `has_missing_conversations`).
 *
 *    Two consequences that must hold wherever this value is read:
 *     · no percentage — the denominator is known to be false, so any `n / total`
 *       would be a fabricated number (progress.ts refuses it);
 *     · the wording has to say what *is* known: "at least N listed".
 */
export type TotalSource = 'response-total' | 'unknown' | 'contradicted';

/** The counting window for the daily quota, split by UTC date. */
export interface DailyCounter {
  /** YYYY-MM-DD（UTC） */
  day: string;
  count: number;
  /**
   * 🔴 W16 · **This day's quota, drawn once when the day rolled over.**
   *
   * The cap used to be the constant `pace.detail.maxPerDay = 200`, which meant
   * the leg published exactly the same ceiling every single day — the most
   * predictable number it has. It is now drawn uniformly from
   * `[DAILY_CAP_MIN, DAILY_CAP_MAX]` (lib/backfill/pace.ts) at the moment
   * `day` changes, so it is irregular *across* days and perfectly stable
   * *within* one:
   *
   *  · it is persisted here rather than remembered in a module variable, so a
   *    restart re-reads the cap it already had instead of drawing a new one —
   *    otherwise a user who restarts the browser often enough would keep
   *    re-rolling and, with enough restarts, sit at the top of the range;
   *  · the value actually enforced is `min(cap ?? maxPerDay, maxPerDay)`, so a
   *    stored cap can never exceed the plan's ceiling and an explicit small
   *    `maxPerDay` is never raised by the draw;
   *  · optional: a state written before W16 has no `cap` and reads back as
   *    undefined ⇒ "no cap was drawn for this day", and the plan's own ceiling
   *    applies for the rest of that day. No version bump, no progress lost.
   */
  cap?: number;
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
  enumCursor: {
    offset: number;
    complete: boolean;
    cursor?: number | null;
    /**
     * 🔴 W21 · **The opaque page token**, for a platform whose cursor is not a
     * number (Grok's `nextPageToken`, which the sources describe as an echo of
     * the last conversation id on the page).
     *
     * A field of its own rather than a widened `cursor`: the two are different
     * kinds of thing (one may be min/max-ed and reasoned about, the other may only
     * be handed back unread), and a union type would invite arithmetic on a value
     * that has no arithmetic. `null`/absent = no token yet = request the first
     * page, which is how every state written before W21 reads back — no version
     * bump, no progress lost. A platform is one mode or the other, never both.
     */
    token?: string | null;
    truncated?: EnumTruncation;
  };
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

/**
 * 🔴 W18 · **The header: the state without the debt ids.**
 *
 * It is the whole of what `storage.local` holds now, and it is written exactly as
 * often as the old whole state was — the point of the split is not to write less
 * often, it is to stop writing the 7,391 ids that did not change.
 *
 * `pendingCount` / `archivedCount` are named `…Count` rather than `pending` /
 * `archived` deliberately: in `BackfillState` those two names are *lists of ids*
 * and here they would be *numbers*, and a field whose unit depends on which type
 * you happen to be holding is exactly the silent confusion this repository's
 * invariants are about. The type checker cannot help you if the name lies.
 *
 * 🔴 The counts are **not** the authority. The debt store is, and `loadState`
 *    re-derives both counts from it on every load (see `lib/backfill/ledger.ts`).
 *    They are here so the popup can render one line of progress from a plain
 *    `storage.local` snapshot without opening IndexedDB.
 *
 * Optional fields carry the same meaning and the same compatibility rules as the
 * identical fields on `BackfillState`; they are spelled out here rather than
 * inherited so that a change to one is forced to be a change to the other.
 */
export interface BackfillHeader {
  v: typeof BACKFILL_STATE_VERSION;
  platform: string;
  scope: string;
  totalKnown: number | null;
  totalSource: TotalSource;
  enumCursor: { offset: number; complete: boolean; cursor?: number | null; token?: string | null; truncated?: EnumTruncation };
  /** How many debts were still owed when this header was written. */
  pendingCount: number;
  /** How many conversations had been settled when this header was written. */
  archivedCount: number;
  detailOutcomes?: DetailOutcomeRecord[];
  detailToday: DailyCounter;
  lastFetchAt?: { enumerate: number | null; detail: number | null };
  failures?: import('./failures').FailureEntry[];
  failuresDropped?: number;
  halted: HaltRecord | null;
}

/** The storage key of the header. Same cs_* prefix family as the badge; no new permission. */
export function stateKey(platform: string, scope: string): string {
  return `cs_backfill_v${BACKFILL_STATE_VERSION}:${platform}:${scope}`;
}

/**
 * The key the pre-W18 layout used. Read once per scope by the migration; never
 * written by anything else.
 */
export function legacyStateKey(platform: string, scope: string): string {
  return `cs_backfill_v${LEGACY_STATE_VERSION}:${platform}:${scope}`;
}

/** Split the in-memory state into the header that gets persisted and the id sets that do not. */
export function headerOf(state: BackfillState): BackfillHeader {
  return {
    v: BACKFILL_STATE_VERSION,
    platform: state.platform,
    scope: state.scope,
    totalKnown: state.totalKnown,
    totalSource: state.totalSource,
    enumCursor: state.enumCursor,
    pendingCount: state.pending.length,
    archivedCount: state.archived.length,
    detailOutcomes: state.detailOutcomes,
    detailToday: state.detailToday,
    lastFetchAt: state.lastFetchAt,
    failures: state.failures,
    failuresDropped: state.failuresDropped,
    halted: state.halted,
  };
}

/**
 * Put a header back together with the id sets that were read from the debt store.
 * The counts are **assigned from the sets**, never taken from the header: a header
 * whose counts disagree with the debt store is a header written before a crash,
 * and the store is the one that knows.
 */
export function stateFrom(header: BackfillHeader, pending: string[], archived: string[]): BackfillState {
  return {
    v: BACKFILL_STATE_VERSION,
    platform: header.platform,
    scope: header.scope,
    totalKnown: header.totalKnown,
    totalSource: header.totalSource,
    enumCursor: header.enumCursor,
    pending,
    archived,
    detailOutcomes: header.detailOutcomes ?? [],
    detailToday: header.detailToday,
    lastFetchAt: header.lastFetchAt,
    failures: header.failures,
    failuresDropped: header.failuresDropped,
    halted: header.halted,
  };
}

export function isHeader(value: unknown): value is BackfillHeader {
  if (!value || typeof value !== 'object') return false;
  // `pending`/`archived` are read off the raw record on purpose: the whole point
  // of the last two checks is to refuse a record that carries id arrays, and a
  // typed view of a header has no such properties to look at.
  const h = value as Partial<BackfillHeader> & Record<string, unknown>;
  return (
    h.v === BACKFILL_STATE_VERSION
    && typeof h.platform === 'string'
    && typeof h.scope === 'string'
    && typeof h.pendingCount === 'number'
    && typeof h.archivedCount === 'number'
    && !Array.isArray(h.pending)
    && !Array.isArray(h.archived)
    && typeof h.enumCursor === 'object'
    && h.enumCursor !== null
  );
}

/**
 * Is this the layout W18 replaced?
 *
 * 🔴 Everything here is required, including the two arrays: a record that merely
 *    has a `v` of 1 is not a v1 state, and the difference matters — this predicate
 *    is what decides whether the migration may write the debt store, and a false
 *    positive would mean replacing real ids with ones read out of a shape we
 *    guessed at.
 */
export type LegacyBackfillState = Omit<BackfillState, 'v'> & { v: typeof LEGACY_STATE_VERSION };

export function isLegacyState(value: unknown): value is LegacyBackfillState {
  if (!value || typeof value !== 'object') return false;
  const s = value as Partial<LegacyBackfillState>;
  return (
    s.v === LEGACY_STATE_VERSION
    && typeof s.platform === 'string'
    && typeof s.scope === 'string'
    && Array.isArray(s.pending)
    && Array.isArray(s.archived)
    && typeof s.enumCursor === 'object'
    && s.enumCursor !== null
  );
}

export function dayKeyOf(nowMs: number): string {
  return new Date(nowMs).toISOString().slice(0, 10);
}
