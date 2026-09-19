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
  | 'state-unreadable'
  /**
   * 🔴 W31 · **The account has more than one organization and nothing on the page
   * named which one.**
   *
   * This is claude.ai's own shape: every request carries an organization id in its
   * path, the id is not in the page URL, and an account may belong to several. The
   * resolver (lib/backfill/claude-org.ts) reads the id from the page's own
   * requests first and from the `lastActiveOrg` cookie second; this reason is what
   * it produces when neither answered and `GET /api/organizations` listed more
   * than one. Since W31c the resolver runs in the page itself
   * (lib/backfill/claude-page.ts), reached over the backfill tab channel, and
   * **this is the reason the popup shows for it** — its own plain sentence, not
   * the `other` fallback.
   *
   * 🔴 What it is **not**: an error, a rate limit, or "you have no conversations".
   *    It holds **before any list request is issued** — zero conversations were
   *    enumerated, and none was written off. Picking one of the organizations
   *    would read a different organization's history than the page the user is
   *    looking at, and iterating them until one answers is the same mistake with
   *    more requests; so the leg stops, named, and waits for a human.
   *
   * Permanent by construction (`haltClassOf`'s default), and **not re-asked**: the
   * recorded reason is already the answer a second question would reach, so the
   * wake-up does not spend one (`entrypoints/background.ts`'s `scopeRetryDue`).
   * It clears when the resolver has an answer — which arrives by itself the moment
   * the page shows its own organization, i.e. when the user opens a conversation
   * in the organization they meant, and that is exactly what the popup's sentence
   * for this reason tells them to do.
   */
  | 'org-ambiguous'
  /**
   * 🔴 W31 · **No organization could be named at all** — the page showed none, the
   * cookie held none, and `GET /api/organizations` answered with an empty list or
   * with a body that is not that list.
   *
   * A separate reason from 'org-ambiguous' because the two are different facts
   * about the account: there, several candidates and no way to choose; here, not
   * one candidate. Both hold before any request for conversation data, and neither
   * may be recorded as "no conversations".
   *
   * 🔴 The engine also produces it for a run whose scope is missing or is the
   *    `'default'` sentinel — a target registered before the organization was
   *    known — so this is the reason a scoped platform shows whenever the
   *    resolution has not produced an organization yet, whichever way it got
   *    there. Its popup sentence names the action that fixes both cases: open a
   *    conversation on the platform once.
   */
  | 'org-unresolved'
  /**
   * 🔴 W45 · **This scope's header records debts the debt store no longer holds.**
   *
   * Measured on a real logged-in Chrome (2026-09-19): the header at
   * `cs_backfill_v2:chatgpt:default` said 7,736 pending / 17 archived while the
   * `debts` store held **0 rows**. The store used to be keyed by scope alone, and
   * three platforms share the scope string `default` — so the ordinary open of a
   * *fresh, empty* ledger for deepseek or gemini computed every one of chatgpt's
   * 7,736 rows as a deletion and removed them. The run then loaded an empty
   * `pending`, returned `queue-empty`, and reported `ran` on every alarm tick for
   * four hours while fetching nothing at all.
   *
   * Why it must be its own reason rather than `queue-empty`: an empty queue and a
   * **destroyed** one are different facts, and collapsing them is what made four
   * hours of "ran, nothing happened" look like success. The wording is about the
   * disagreement itself, not about its cause: rows are provably gone, and how they
   * went is not something this record can know.
   *
   * 🔴 The comparison is one-directional on purpose. `Ledger.save` writes the debt
   *    store *before* the header, so a worker killed between the two leaves a
   *    store that is **ahead** of the header's counts — which is normal, because
   *    the counts are recomputed from the store on every load. Only the store
   *    holding *fewer* debts than the header recorded is a loss. A naive equality
   *    test would turn every ordinary crash into a refusal.
   *
   * What happens when it is seen: the run refuses to fetch (nothing is fetched
   * against a debt set that lost rows), the header is repaired so the scope can be
   * filled again (`recoverLedgerLoss` in lib/backfill/ledger.ts), and the popup
   * says so.
   */
  | 'ledger-mismatch';

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
 *  · 'ledger-mismatch' — W45. A **permanent** record, and not because waiting would
 *    not help: the repair is a one-shot header reset, not a retry, and it is driven
 *    by the state this reason describes rather than by a clock. Classifying it
 *    transient would put it on the backoff ladder and have the popup promise a
 *    self-resuming wait, which is a different story from the one that is true —
 *    that the leg found a provable loss, refused to fetch against it, and reset the
 *    scope so the next run can read its list again.
 */
export function haltClassOf(reason: HaltReason): HaltClass {
  return reason === 'transport-error' || reason === 'rate-limited' ? 'transient' : 'permanent';
}

/**
 * 🔴 W44 · **What a halt is a judgement about.** The question `HaltClass` does not
 * ask, and whose absence let a fact about a build outlive the build.
 *
 * `HaltClass` says *when* a stop may be re-asked (never / after a backoff). It does
 * not say *what would have to change* for the answer to change — and that is the
 * distinction W44 needed. A record saying "this platform has no backfill
 * enumeration yet" is not a fact about the account or the platform; it is a fact
 * about the code that wrote it, and it stopped being true the moment the plan
 * table gained that platform. Measured on a real logged-in Chrome (2026-09-19):
 * three such records, 53-61 minutes old, written by earlier builds, held down two
 * platforms whose plans this build ships — the popup said they can backfill while
 * the engine refused to run them.
 *
 * So every reason is classified by its **subject**:
 *
 *   · 'capability' — a statement about what **this build** can do. Its whole
 *     truth condition is the plan table, so it can be checked against the plan
 *     table and expires by itself when that table changes. See `haltSubjectOf`.
 *   · 'upstream'   — a statement about a **response that arrived** (or did not):
 *     the bytes are not a shape we know, the body came back empty, the platform
 *     said "not now", the transport threw. Nothing this build ships can make one
 *     of these untrue; only the platform (or a human reading the wire) can.
 *   · 'account'    — a statement about the **user's account**: which organization
 *     it belongs to, and whether one could be named at all. A human action on the
 *     platform is what changes it, and the popup names that action.
 *   · 'storage'    — a statement about **our own persisted record**: there is no
 *     store, or there is one and it cannot be read, or it disagrees with what it
 *     says about its own debts. A human looks at the storage; waiting and running
 *     again cannot repair any of them.
 *
 * 🔴 **The switch below has no `default`, on purpose.** Adding a value to
 *    `HaltReason` fails `tsc --noEmit` until someone says what that new stop is a
 *    judgement about — which is the one thing this file cannot leave to a reviewer,
 *    because the failure mode of getting it wrong is invisible: a permanent stop
 *    that quietly expires, or a stop that quietly never does.
 *
 * 🔴 A reason may be classified 'capability' **only if the engine raises it purely
 *    from the plan-table lookup**. That is what makes the expiry check sound: the
 *    marker is computed from the same lookup the judgement came from, so the two
 *    cannot disagree. A future 'capability' reason whose condition lives anywhere
 *    else would need a marker of its own rather than this one.
 */
export type HaltSubject = 'capability' | 'upstream' | 'account' | 'storage';

export function haltSubjectOf(reason: HaltReason): HaltSubject {
  switch (reason) {
    // The two the plan table decides, and nothing else does. Both fire before the
    // request they describe would have been sent, so a re-decision is free of
    // requests *and* of consequences.
    case 'unsupported-platform':
    case 'detail-unsupported':
      return 'capability';

    // Arrived-and-unreadable, arrived-empty, refused, or never arrived. The build
    // that reads them next is not what makes them true or false.
    case 'shape-changed':
    case 'detail-empty-unverified':
    case 'rate-limited':
    case 'transport-error':
      return 'upstream';

    case 'org-ambiguous':
    case 'org-unresolved':
      return 'account';

    // No store, an unreadable store, a store that lost rows, and "there is nowhere
    // to persist a retry" — all four are about the record itself.
    case 'storage-unavailable':
    case 'state-unreadable':
    case 'ledger-mismatch':
      return 'storage';
  }
}

/**
 * 🔴 W44 · **How much of a platform this build can backfill.** The value a
 * capability-class halt is judged against, and the only thing it is judged against.
 *
 * Why a three-value answer and not a plan revision or a hand-kept version number:
 * the record's own claim is about **which of the two segments exist** — "we cannot
 * even list your conversations" ('none'), "we can list them but cannot fetch a
 * body" ('list-only'), "both segments exist" ('full') — and this is that claim,
 * derived rather than declared. A version number bumped by hand is exactly the
 * mechanism that fails silently: someone adds a plan, ships, and the number they
 * forgot to bump means the record still applies.
 *
 * 🔴 The derivation lives in `lib/backfill/enumerate.ts` (`capabilityOf`, which
 *    reads the same `backfillPlanFor` / `canBackfillDetail` pair the halt itself
 *    is raised from). This type lives here, with no import of its own, so that
 *    `types.ts` keeps its promise of doing no I/O and depending on nothing — and
 *    so a record's shape cannot drift from the classification above.
 */
export type BackfillCapability = 'none' | 'list-only' | 'full';

/**
 * 🔴 W44 · **The value a record written before this change carries: it did not
 * say.** Not a fourth capability — a fourth *kind of statement*, and the two must
 * not be rounded into each other.
 *
 * A record with no marker cannot be checked against anything: whichever build
 * wrote it, it was written in a world where nothing asked the question. So the
 * migration rule is per **class**, not per value (see `haltStillApplies`):
 *
 *   · a **capability**-class record without a marker is treated as stale, and the
 *     leg re-decides. The cost is bounded and is stated where it is paid
 *     (`engine.ts`'s expiry branch): at most one run per legacy record, which
 *     re-asks the plan table and, if the answer is still no, writes the same halt
 *     back — now marked, so it cannot happen twice. Both reasons fire before the
 *     request they are about, so the re-decision sends nothing in the 'none' case
 *     and only reads the list it can already read in the 'list-only' case;
 *   · an **account**, **upstream** or **storage** record without a marker keeps
 *     its full force. Reading "unmarked" as "expired" for those would clear real
 *     halts — a platform whose wire changed, an account whose organization could
 *     not be named — and that is worse than the defect this task exists to fix.
 */
export const CAPABILITY_UNMARKED = 'unmarked';

/**
 * 🔴 W44 · **Does a stored record still apply to a build whose capability is
 * `capability`?**
 *
 * One function, in one place, because two callers ask this question and the
 * failure mode of two answers is the one this project keeps meeting: two lists
 * that disagree. The engine asks it before refusing to run; the alarm's
 * `scopeRetryDue` asks it before deciding that a scope is not worth asking the
 * page about again. The popup is **not** a third caller: it reads the
 * `haltExpired` record the engine already wrote, so there is no second judgement
 * for it to get wrong.
 *
 * A record for any other subject **always applies** — this function is not a
 * second expiry mechanism, and it deliberately answers `true` for the account,
 * upstream and storage classes so that a caller cannot accidentally treat one of
 * them as capability-class by forgetting to check the subject first.
 */
export function haltStillApplies(record: HaltRecord, capability: BackfillCapability): boolean {
  if (haltSubjectOf(record.reason) !== 'capability') return true;
  return (record.capability ?? CAPABILITY_UNMARKED) === capability;
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
  /**
   * 🔴 W44 · **What this build could do when the record was written** — present on
   * capability-class reasons (`unsupported-platform`, `detail-unsupported`) and on
   * nothing else. Absent on every other reason, and absent on a record written
   * before W44 (read as `CAPABILITY_UNMARKED`).
   *
   * Why the field is on the record rather than in a key of its own: the same
   * reason `retryAt` is (W13). The popup's question is "is this leg stopped, and
   * why", and a marker in a parallel key could disagree with `halted` about
   * whether the record it describes is still in force. One record, one answer.
   *
   * 🔴 Why this cannot silently fail to change when the capability does: it is not
   *    maintained by hand anywhere. `engine.ts` computes it from `opts.plans ??
   *    backfillPlanFor` — the **same lookup function** whose `null` answer raises
   *    'unsupported-platform' and whose plan raises 'detail-unsupported' — and
   *    `capabilityOf` reads that plan's own `canBackfillDetail` rather than a
   *    separate table. Add a plan to `PLANS` and the next record says 'full'
   *    without anyone editing a constant; there is no number to forget to bump.
   *    The two facts that must agree, `halted` and `capability`, are written by
   *    the same statement for that reason.
   */
  capability?: BackfillCapability;
}

/**
 * 🔴 W44 · **A stored halt stopped applying because the build changed — the trace
 * that says so.**
 *
 * Why this has to exist, and has to outlive the record it replaces: the moment a
 * capability-class halt expires the engine clears it, and a cleared record is
 * *silence*. A user who saw "this platform's history cannot be backfilled yet"
 * and later sees the platform backfilling has been told nothing about why the
 * sentence disappeared — and silence in that direction is the same defect as the
 * one this task fixes, wearing a different coat (a leg that appears to start by
 * itself for no reason). So the expiry leaves a record the popup can read.
 *
 * 🔴 It is **durable**, on the header rather than in a run report, for the same
 *    reason W45's `relisted` is: the run that clears the record is gone from
 *    memory by the time anyone opens a popup, and what has to survive is the fact
 *    that it ever happened. It is overwritten by the next expiry for the same
 *    scope, so a scope can hold one of these at a time.
 *
 * The four values are observations, not estimates: which reason the record named,
 * when the build that wrote it recorded that (`recordedAt`), what it said its
 * capability was — or `CAPABILITY_UNMARKED` when it did not say — and what this
 * build's capability is (`capability`), which is the whole reason the record no
 * longer applies.
 */
export interface HaltExpiry {
  /** The reason the expired record named. */
  reason: HaltReason;
  /** When that record was written, by the build that judged it. */
  recordedAt: number;
  /** What the record said the build could do, or `CAPABILITY_UNMARKED` when it did not say. */
  judgedAgainst: BackfillCapability | typeof CAPABILITY_UNMARKED;
  /** What this build can do — the value that made the record stop applying. */
  capability: BackfillCapability;
  /** When this build stopped applying it. */
  clearedAt: number;
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
  /**
   * 🔴 W45 · **The last time this scope's debt set was found behind its own header
   * and the enumeration cursor was reset so the list would be read again.**
   *
   * It exists because the repair has to be *sayable*. The refusal itself is a halt
   * (`ledger-mismatch`) and a run report, and both are gone by the time the user
   * opens the popup; a scope that lost 7,736 ids and then quietly refilled would
   * leave no trace that anything had ever been wrong. This is that trace, and it
   * survives in the header.
   *
   * `recorded` and `held` are the two numbers the disagreement was measured with:
   * how many conversation ids the header recorded, and how many the store actually
   * held for this platform and scope. They are **observations, not estimates** —
   * two reads and one subtraction on numbers that were really there — and they are
   * the totals rather than the pending/archived split because the split is not
   * something this record can know (see `DebtLoss` in lib/backfill/ledger.ts).
   *
   * 🔴 What it does **not** claim: why the ids went, or which of them were already
   *    archived. The count is not the ids, and the ids are exactly what was lost —
   *    so an already-archived conversation cannot be told from one that was never
   *    fetched, and re-listing will enqueue it again. That is stated in the popup
   *    sentence rather than hidden here.
   *
   * Optional: a state written before W45 has no such field and reads back as
   * undefined ⇒ "this scope has never been re-listed", byte-identical to before,
   * with no version bump and no progress invalidated.
   */
  relisted?: { at: number; recorded: number; held: number };
  /**
   * 🔴 W44 · **The last capability-class halt that stopped applying because this
   * build's capability is not the one the record was judged against.** See
   * `HaltExpiry`.
   *
   * Optional: a state written before W44 has no such field and reads back as
   * undefined ⇒ "no stored halt has ever expired here", byte-identical to before,
   * with no version bump and no progress invalidated.
   */
  haltExpired?: HaltExpiry;
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
  /** W45 · Same meaning and same compatibility rule as `BackfillState.relisted`; spelled out here so a change to one is forced to be a change to the other. */
  relisted?: { at: number; recorded: number; held: number };
  /** W44 · Same meaning and same compatibility rule as `BackfillState.haltExpired`; spelled out here so a change to one is forced to be a change to the other. */
  haltExpired?: HaltExpiry;
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
    relisted: state.relisted,
    haltExpired: state.haltExpired,
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
    relisted: header.relisted,
    haltExpired: header.haltExpired,
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
