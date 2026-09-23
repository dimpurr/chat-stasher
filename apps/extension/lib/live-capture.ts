/**
 * W69 · **When a live capture last arrived, per platform — and what that is
 * evidence of.**
 *
 * ## Why this file exists
 *
 * W68 measured the ChatGPT / Grok / Kimi pages and could answer `hook-was-replaced`
 * completely — the wrapper that took each global, the value it captured, the file
 * its source lives in — and could not answer the one question the user actually
 * has: **is a conversation I open there being archived?** Every instrument the
 * extension already had was silent on it:
 *
 *   · `cs_hook_v1:<origin>` (`lib/hook-status.ts`) records what a page observed
 *     about its own hook. It is an observation, never a diagnosis, and its own
 *     header says so;
 *   · `cs_last_delivered_v1` (`lib/recapture.ts`) maps a delivery name to a
 *     sha256. It is a duplicate-suppression store: **it has no time field at
 *     all**, so no sequence of its values answers "when";
 *   · the stage directory belongs to the CLI and holds more than browser
 *     deliveries, so it cannot be read as a per-platform capture log either.
 *
 * So "did a live capture arrive at time T" was a **gap in the record**, not a
 * negative result. This module is that gap closed: one row per platform, written
 * at the only place a live capture is decided to be stored
 * (`handleCaptured`, `entrypoints/background.ts`), holding a time and a count and
 * nothing else.
 *
 * ## What is recorded, and what is deliberately not
 *
 *  · `at` — when a live capture from this platform **was last confirmed to be in
 *    the archive** (see below: that is one fact, reached two ways).
 *  · `count` — how many captures from this platform are on record as **newly
 *    stored**.
 *
 * Nothing else: no URL, no conversation id, no title, no body, no size. The
 * platform id is the one identifier needed to say which row an answer is about,
 * and it is already in the platform table this extension ships with.
 *
 * 🔴 **Only a stored capture is an arrival.** The writer is called where
 *    `handleCaptured` returns `saved: true`, which that function's own comment
 *    defines as the only value that counts as "it was stored". A capture that was
 *    merely queued, rejected or refused is **not** an arrival — recording it would
 *    make "a conversation from this platform reached the archive" true of a
 *    conversation that did not. `status: 'unchanged'` **is** an arrival: the page
 *    produced a capture and the archive already held exactly that copy, which is
 *    a measurement of the whole path (page → bridge → background → host →
 *    archive) just as a fresh ack is. Not recording it would freeze the row on a
 *    page that re-sends an unchanged conversation on every view — which is what
 *    ChatGPT was measured doing (lib/recapture.ts's header) — and the row would
 *    then read as stale while captures were arriving continuously.
 *
 * 🔴 **But only a *delivered* capture raises the count, because only a delivery
 *    is a store.** An unchanged re-send is the page repeating itself: ChatGPT
 *    re-sends the whole conversation on every view, so counting arrivals made one
 *    conversation read as many stored, and the popup said so. So the two fields
 *    answer two different questions, and the popup's sentence names each:
 *    `at` says when a capture from this platform was last seen to be in the
 *    archive, `count` says how much is on record as having been put there.
 *
 * 🔴 **A platform with no row is not a platform with zero captures.** No writer
 *    creates a row out of nothing and no reader invents one, so absence stays
 *    what it is: we have nothing on record there. That distinction is this
 *    project's first invariant and it is load-bearing here, because the row's
 *    whole purpose is to be evidence.
 *
 *    🔴 W69b · **A `count` of zero is a row, and is not that absence.** It is
 *       reachable: a delivery whose row write failed leaves no record of itself,
 *       so a later unchanged arrival for that platform can be the first arrival
 *       the row ever sees, and nothing was newly stored *by that arrival*. It
 *       prints as it is. Rounding it up to one would claim a store that this
 *       record cannot show, which is the failure the count was corrected for; and
 *       the field is explicitly "what is on record", so a zero here says "nothing
 *       new is on record" and not "nothing ever arrived" — the row's own `at` is
 *       printed beside it and says otherwise.
 *
 * ## What a row is evidence of — and what it is not
 *
 * `captureVerdict` turns "what a page observed about itself" plus "what is on
 * record for that platform" into one of three verdicts. The rule, in order, with
 * the reason each step is where it is:
 *
 *  0. **The record's current observation is its LATEST one, and the verdict is
 *     decided from that alone.** The record is a history (`lib/hook-status.ts`
 *     keeps one entry per reason, with the first and last time it was seen), and
 *     a page that stays in one state re-sends that state every few seconds. Two
 *     things follow, and both are W69b fixes:
 *
 *     🔴 **A reason the record holds is not necessarily the state the page is in
 *        now.** Reading the record as a set — "some reason means our hook is not
 *        in effect" — made a superseded `hook-did-not-run` outvote a later
 *        `hook-was-replaced` *forever*: on the very page this file was written
 *        for, the bridge's fallback probe goes unanswered (because the hook is
 *        ours-but-replaced and refuses to answer for a page it no longer wraps),
 *        files that inference 100 ms after the page's own report, and older
 *        reasons are never removed. The page then re-sends `hook-was-replaced`
 *        every 5 s, and the record's `at` moves with it, while the verdict stayed
 *        `not-working` on an observation the page had already superseded.
 *        So the state is the observation with the greatest time, and the two
 *        below are read from it and from nothing else.
 *
 *     🔴 **The state began when that observation was FIRST made, not when it was
 *        last re-sent.** Comparing a capture against the record's `at` made
 *        `working` last 5 seconds: a capture that arrived after the identity
 *        change was evidence until the page re-sent the same observation, and
 *        then the row looked older than the observation and the verdict fell back
 *        to `unknown`. So the comparison is against the current observation's
 *        `since` — the moment it began — and the capture keeps counting until
 *        something *newer* says otherwise.
 *
 *  1. **A capture at or after the moment that observation began ⇒ `working`.** A
 *     stored conversation is a *measurement* of the whole path; the observation is
 *     an inference from one page's identity check. A measurement outranks an
 *     inference, so this step runs first — including over an observation that
 *     says our hook is not in that page at all.
 *     🔴 Its scope is the **platform**, and that is why the sentence built from it
 *        says "from this platform". The row is kept per platform while the
 *        observation is per origin, so a capture may have come from another tab of
 *        the same platform. The verdict must not claim *this* document delivered
 *        it, and neither must the wording.
 *  2. **Otherwise, a current observation that says our hook is not in effect on
 *     that page ⇒ `not-working`.** `hook-did-not-run` says no copy of our hook ran
 *     in the document; `hook-did-not-take` says it ran and the document's own
 *     global refused the patch. Both mean the page's transport is not ours, so
 *     nothing there is captured — and there is no measurement to outweigh them,
 *     because step 1 did not fire.
 *  3. **Otherwise ⇒ `unknown`.** That is the case this file was written for: an
 *     identity change, and no capture on record since it. `hook-was-replaced`
 *     says the wrapper we installed is no longer the page's global. It does not
 *     say the page stopped capturing: W68 measured every replacement on those
 *     pages forwarding to the value it had captured, which was ours. So the state
 *     of capture is *undetermined* by that observation, and with nothing on record
 *     to settle it, this is unknown — never "working" and never "broken".
 *
 * 🔴 **The rule is a function of two facts and knows nothing else.** It does not
 *    read a cause into an absence: a platform with no row and no observation
 *    produces no verdict at all, and no verdict is built from "enough time has
 *    passed" or "the user probably visited the page".
 */

import type { BackfillStore } from './backfill/store';
import {
  HOOK_OBSERVATIONS,
  HOOK_REASON_DID_NOT_RUN,
  HOOK_REASON_DID_NOT_TAKE,
  HOOK_REASON_WAS_REPLACED,
  type HookObservation,
} from './contract';
import type { HookObservationRecord, HookStatusRecord } from './hook-status';

/** Storage key prefix. One row per platform. */
export const LIVE_CAPTURE_KEY_PREFIX = 'cs_live_capture_v1:';

/** One platform's record of live captures that reached the archive. */
export interface LiveCaptureRecord {
  platform: string;
  /**
   * When a live capture from this platform was last **confirmed to be in the
   * archive** (`Date.now()` ms) — not when the row started, and not when the last
   * one was attempted. Both arrival kinds confirm it: a delivered capture has
   * just been stored, and an unchanged one is a copy the archive already held.
   * The time only moves forward (see `mergeLiveCapture`).
   */
  at: number;
  /**
   * How many captures from this platform are on record as **newly stored** — the
   * `status: 'delivered'` arrivals only. An unchanged re-send moves `at` and
   * leaves this alone: the page produced another capture, but the archive did not
   * gain another copy, and a count of arrivals read as a count of stored
   * conversations on every page that re-sends what it already sent.
   */
  count: number;
}

export function liveCaptureKey(platform: string): string {
  return `${LIVE_CAPTURE_KEY_PREFIX}${platform}`;
}

/** Strict enough that junk in storage is never read as a record. */
export function looksLikeLiveCapture(value: unknown): value is LiveCaptureRecord {
  if (!value || typeof value !== 'object') return false;
  const record = value as Record<string, unknown>;
  if (typeof record.platform !== 'string' || record.platform.length === 0) return false;
  if (typeof record.at !== 'number' || !Number.isFinite(record.at)) return false;
  // 🔴 W69b · A whole number, zero or more. Not `>= 1`: zero is a state this
  //    record can honestly be in (see the header), and a reader that refused it
  //    would turn a row we wrote into "nothing on record". Requiring it to be an
  //    *integer* is the half of the old check that still earns its place — a
  //    fractional count is not a count.
  return typeof record.count === 'number' && Number.isInteger(record.count) && record.count >= 0;
}

/**
 * Every live-capture row in a storage snapshot, newest first.
 *
 * 🔴 A value under the prefix that is not a record is **not** repaired, guessed
 *    at, or turned into a row holding zero: it is left alone and left out of this
 *    list, and the popup then says nothing rather than saying something invented
 *    (the same rule `hookStatusOf` in lib/hook-status.ts and `backfillStateEntries`
 *    in lib/popup-view.ts follow). A key whose platform disagrees with the row
 *    inside it is dropped for the same reason — the two are one fact, and a row
 *    that contradicts itself is not a row to show a user.
 */
export function liveCaptureOf(snapshot: Record<string, unknown> | null): LiveCaptureRecord[] {
  if (!snapshot) return [];
  const out: LiveCaptureRecord[] = [];
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(LIVE_CAPTURE_KEY_PREFIX)) continue;
    if (!looksLikeLiveCapture(value)) continue;
    if (key !== liveCaptureKey(value.platform)) continue;
    out.push(value);
  }
  out.sort((a, b) => b.at - a.at);
  return out;
}

/** The row for one platform out of a snapshot read, or `null` when there is none. */
export function liveCaptureFor(
  rows: readonly LiveCaptureRecord[] | null | undefined,
  platform: string,
): LiveCaptureRecord | null {
  if (!rows) return null;
  for (const row of rows) if (row.platform === platform) return row;
  return null;
}

/**
 * Merge one arrival into a row (or start one). **Pure**: the caller persists what
 * comes back.
 *
 * The time only ever moves forward and the count only ever grows while the
 * platform stays the same. A row stored under a different platform is not this
 * platform's row; it is replaced rather than merged, the same way
 * `mergeHookObservation` treats an origin it does not match — two platforms
 * merged into one row would be a count that measures neither.
 *
 * 🔴 W69b · **`newlyStored` is the difference between the two fields.** Every
 *    arrival moves `at`; only one that the host acknowledged raises `count`. See
 *    `LiveCaptureRecord.count` and this file's header for why the two are not the
 *    same question.
 */
export function mergeLiveCapture(
  existing: LiveCaptureRecord | null,
  arrival: { platform: string; at: number; newlyStored: boolean },
): LiveCaptureRecord {
  const base = existing && existing.platform === arrival.platform ? existing : null;
  return {
    platform: arrival.platform,
    // A clock that moved backwards (an NTP correction, a suspend/resume) must not
    // make the row claim a capture is older than one already recorded: the row
    // answers "when did we last see one", and that cannot go backwards.
    at: base ? Math.max(base.at, arrival.at) : arrival.at,
    count: (base?.count ?? 0) + (arrival.newlyStored ? 1 : 0),
  };
}

/**
 * 🔴 Serialises the read-merge-write.
 *
 * MV3 delivers `chat-captured` messages while an earlier one is still being
 * handled — the listener returns `true` and keeps the channel open — so two
 * `recordLiveCapture` calls can interleave, both read the same row, and both hear
 * "count was one" on the way back. One arrival would then be missing from a count
 * whose whole job is to be a measurement. Chaining them makes each arrival observe
 * the previous one's write.
 *
 * The chain is module state, so it serialises within one service-worker instance,
 * which is the only place two of these can race: a reclaimed worker starts a new
 * chain with nothing in flight. A rejected link is caught before it is chained, so
 * one failed write cannot poison every later one.
 */
let writeChain: Promise<void> = Promise.resolve();

/**
 * Write one arrival down.
 *
 * 🔴 Best-effort, like `recordHookStatus` and for the same reason: a record about
 *    a capture must never become the reason the capture's own path breaks. A store
 *    that cannot be written is logged and the row stays as it was — the capture
 *    itself has already been stored by the time this is called.
 *
 * 🔴 A platform this cannot name is not written under a guess. The key is derived
 *    from the platform id, and an id that is not known produces no key rather than
 *    an empty suffix that every nameless capture would share.
 */
export async function recordLiveCapture(
  store: BackfillStore | null,
  arrival: { platform: string | null; at: number; newlyStored: boolean },
): Promise<void> {
  if (!store || !arrival.platform) return;
  const platform = arrival.platform;
  const key = liveCaptureKey(platform);
  const run = async (): Promise<void> => {
    const stored = await store.load(key);
    await store.save(
      key,
      mergeLiveCapture(looksLikeLiveCapture(stored) ? stored : null, {
        platform,
        at: arrival.at,
        newlyStored: arrival.newlyStored,
      }),
    );
  };
  const next = writeChain.then(run, run);
  writeChain = next.catch((err: unknown) => {
    console.warn('[chat-stasher] live-capture record write failed', (err as Error).message);
  });
  return writeChain;
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

export const CAPTURE_VERDICT_WORKING = 'working';
export const CAPTURE_VERDICT_NOT_WORKING = 'not-working';
export const CAPTURE_VERDICT_UNKNOWN = 'unknown';

export const CAPTURE_VERDICTS = [
  CAPTURE_VERDICT_WORKING,
  CAPTURE_VERDICT_NOT_WORKING,
  CAPTURE_VERDICT_UNKNOWN,
] as const;

export type CaptureVerdict = (typeof CAPTURE_VERDICTS)[number];

/**
 * 🔴 **The two observations that say our hook is not in that page.** The rest of
 *    the closed set says something else, and `HOOK_OBSERVATIONS` is a closed set
 *    precisely so this table can be total: a new member fails to compile here
 *    until someone says which of the two it is.
 */
const OBSERVATIONS_THAT_MEAN_NOT_IN_EFFECT: Record<HookObservation, boolean> = {
  [HOOK_REASON_DID_NOT_RUN]: true,
  [HOOK_REASON_DID_NOT_TAKE]: true,
  // An identity change: the wrapper that replaced ours forwards to the value it
  // captured, which was ours (W68). Not, on its own, a statement about capture.
  [HOOK_REASON_WAS_REPLACED]: false,
};

/** The observation a hook record says the page is in **now**, and since when. */
export interface CurrentObservation {
  reason: HookObservation;
  /** When that observation was last made (`HookObservationRecord.at`). */
  at: number;
  /**
   * When it was first made — the moment the state it describes began. Falls back
   * to `at` for an entry that carries no first time; see
   * `HookObservationRecord.since`. Never later than `at`, so a reader may compare
   * a time against it and claim no more than we know.
   */
  since: number;
}

/**
 * 🔴 W69b · **Which observation the page is in now: the latest one.**
 *
 * The record keeps one entry per reason ever reported, so it is a history, not a
 * snapshot — a page that stays in one state re-sends that state every few
 * seconds, and an older entry is a state the page has since moved out of. The
 * current state is therefore the entry with the greatest time, and it is one
 * definition used by everything that reads the record for a state: the verdict
 * below, and the popup's wording (which prints the time the current observation
 * began). `lib/hook-status.ts`'s `mergeHookObservation` keeps the record's own
 * `at` equal to this maximum.
 *
 * Ties (two reasons observed at the same millisecond) are broken by the closed
 * set's own order, so the answer is a property of the vocabulary and not of the
 * order the entries happen to sit in — and that order puts `hook-was-replaced`
 * last, so a tie cannot make the popup call a page broken on the strength of an
 * ordering accident: `not-working` needs the record's newest time to say so by
 * itself. `null` for a record with no reasons at all — which
 * `looksLikeHookStatus` already refuses, so this is the empty case spelled out
 * rather than assumed away.
 */
export function currentObservation(
  reasons: readonly HookObservationRecord[],
): CurrentObservation | null {
  let best: HookObservationRecord | null = null;
  for (const row of reasons) {
    if (best === null || row.at > best.at) {
      best = row;
      continue;
    }
    if (row.at === best.at && HOOK_OBSERVATIONS.indexOf(row.reason) > HOOK_OBSERVATIONS.indexOf(best.reason)) {
      best = row;
    }
  }
  if (best === null) return null;
  // Falling back to `at` is the conservative direction: it is the latest time the
  // reason was seen, so it can only make the state look *younger* than it is, and
  // a younger state means a capture has to be newer to count. Never the other way.
  return { reason: best.reason, at: best.at, since: best.since ?? best.at };
}

/**
 * What the two facts we hold amount to. See this file's header for the rule and
 * the reason each step sits where it does. **Pure**, and the only place a verdict
 * is decided — the popup's wording is chosen from the value this returns and never
 * recomputed there.
 *
 * 🔴 W69b · **Both steps are read from the record's current observation**, not
 *    from the union of everything the record holds: `currentObservation` above
 *    says why, and this file's header has the two failures the union caused.
 *
 * 🔴 Its first argument is the record's **reasons**, not the whole record: the
 *    record's own `at` is the greatest of their times by construction
 *    (`mergeHookObservation`), so taking it as well would be a second expression
 *    of one fact — and a reader of this function cannot tell which of the two a
 *    rule was written against. The popup still prints the record's `at`; that is
 *    its own sentence, not this decision.
 */
export function captureVerdict(
  observed: Pick<HookStatusRecord, 'reasons'>,
  live: Pick<LiveCaptureRecord, 'at'> | null | undefined,
): CaptureVerdict {
  const current = currentObservation(observed.reasons);
  if (current === null) return CAPTURE_VERDICT_UNKNOWN;
  // Against `since`, not `at`: a capture that arrived after this state began is
  // evidence about this state, and it stays evidence while the page re-sends the
  // same observation. See this file's header.
  if (live && Number.isFinite(live.at) && live.at >= current.since) return CAPTURE_VERDICT_WORKING;
  if (OBSERVATIONS_THAT_MEAN_NOT_IN_EFFECT[current.reason]) return CAPTURE_VERDICT_NOT_WORKING;
  return CAPTURE_VERDICT_UNKNOWN;
}
