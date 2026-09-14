/**
 * Honest progress.
 *
 * 🔴 Hard requirement: **when the denominator is unavailable, never show a
 *    percentage.**
 *
 * The test (all of it must hold before a percentage may appear):
 *   1. totalSource === 'response-total' — the denominator comes from the total
 *      the API itself returned, not from our estimate and not from "how many we
 *      have enumerated so far" standing in for it;
 *   2. totalKnown is a finite integer > 0;
 *   3. archived count <= totalKnown (otherwise the denominator is stale or
 *      untrustworthy, and we go back to unknown);
 *   4. 🔴 W10 · the total has not been **disproved by measurement**
 *      (totalSource !== 'contradicted').
 * If any one of those fails we take a no-percentage wording, and that output
 * contains **no '%' character at all**.
 *
 * Why "enumerated so far" is not allowed to be the denominator: while
 * enumeration is still running it keeps growing, so the bar would shoot up to
 * 100% and fall back down — a fake percentage, worse than no progress bar.
 *
 * 🔴 W10 · "we were given no total" and "the total we were given is false" are
 *    two different sentences, and the second one is not a subset of the first:
 *    it has a positive half ("at least N rows are listed") and it must not be
 *    rendered through the `unknownTotal` wording, which says the API provided
 *    nothing. Hence the separate branch and the separate catalog entry.
 *
 * The wording lives in the catalog; this file only decides *which* sentence and
 * with what numbers. `unknownReasonKey` is a message key, not a sentence, so
 * that a progress view computed once is still renderable in either language.
 */

import { t } from '../i18n';
import { haltClassOf, type BackfillState, type HaltRecord } from './types';

/**
 * 🔴 W13 · How long until a transient stop retries, in whole minutes, rounded up
 * (and never negative). Exported because the popup's note and the progress prefix
 * must never give two different numbers for the same moment.
 *
 * A record with no `retryAt` — one written before W13 — is **due now**, so the
 * answer is 0 rather than "unknown": the legacy record is not "we forgot when",
 * it is "no delay was ever decided", and 0 is what the engine will do with it.
 */
export function retryMinutesLeft(halt: HaltRecord, now: number): number {
  if (halt.retryAt === undefined) return 0;
  return Math.max(0, Math.ceil((halt.retryAt - now) / 60_000));
}

export interface ProgressView {
  archived: number;
  pending: number;
  /**
   * 🔴 W10 · How many rows the conversation list has handed over so far — the
   * persisted enumeration cursor. It is a measurement ("that many rows really
   * came back"), and it is what the wording falls back on when the API's own
   * total has been disproved: "at least N listed".
   */
  listed: number;
  /** Only a number when the denominator is trustworthy, otherwise null. */
  totalKnown: number | null;
  /** Integer 0-100; null when untrustworthy. null ⇒ the renderer must not emit '%'. */
  percent: number | null;
  /**
   * Catalog key for why there is no percentage (null when there is one) — so
   * that "we do not know" is itself readable, in the reader's language.
   */
  unknownReasonKey: string | null;
  /**
   * 🔴 W10 · The API's own total was **disproved** by the rows actually listed
   * (state.totalSource === 'contradicted'). Kept next to `percent` because the
   * two answer different questions: `percent === null` says "no percentage",
   * this says "and here is why, specifically: the denominator is a known-false
   * number, not a missing one".
   */
  totalContradicted: boolean;
  halted: BackfillState['halted'];
}

export function computeProgress(state: BackfillState): ProgressView {
  const archived = state.archived.length;
  const pending = state.pending.length;
  const listed = state.enumCursor.offset;

  let percent: number | null = null;
  let unknownReasonKey: string | null = null;
  let totalKnown: number | null = null;
  const totalContradicted = state.totalSource === 'contradicted';

  if (totalContradicted) {
    // 🔴 W10 · The number is not missing, it is **wrong**: more rows came back
    //    than it claims exist. No arithmetic on it may reach the user, so the
    //    view carries neither a total nor a percentage.
    unknownReasonKey = 'progress.reason.totalContradicted';
  } else if (state.totalSource !== 'response-total') {
    unknownReasonKey = 'progress.reason.noTotalFromApi';
  } else if (
    state.totalKnown === null ||
    !Number.isFinite(state.totalKnown) ||
    !Number.isInteger(state.totalKnown) ||
    state.totalKnown <= 0
  ) {
    unknownReasonKey = 'progress.reason.totalNotPositiveInt';
  } else if (archived > state.totalKnown) {
    unknownReasonKey = 'progress.reason.totalExpired';
  } else {
    totalKnown = state.totalKnown;
    percent = Math.floor((archived / totalKnown) * 100);
  }

  return {
    archived,
    pending,
    listed,
    totalKnown,
    percent,
    unknownReasonKey,
    totalContradicted,
    halted: state.halted,
  };
}

/**
 * One line of plain-language progress.
 * With an unknown denominator it reads like: `Archived 12, still owed 30, total
 * unknown (the API did not provide a total)` — say how much is done, how much
 * is still owed, and why there is no denominator. What it does not do is
 * produce a percentage.
 */
export function formatProgress(state: BackfillState, now: number = Date.now()): string {
  const view = computeProgress(state);
  // 🔴 W13 · The prefix has to distinguish the two classes, because "[stopped: …]" is
  //    now false for a transient one: that leg has not stopped, it is waiting out a
  //    backoff and will continue by itself. Saying "stopped" there is exactly the
  //    "unknown recorded as settled" mistake one level up — and it is what made a
  //    torn message channel read as a broken account.
  const head = view.halted
    ? haltClassOf(view.halted.reason) === 'transient'
      ? t('progress.retryPrefix', {
        reason: view.halted.reason,
        attempts: view.halted.attempts ?? 1,
        minutes: retryMinutesLeft(view.halted, now),
      })
      : t('progress.haltedPrefix', { reason: view.halted.reason })
    : '';
  const body =
    view.totalContradicted
      // 🔴 W10 · The disproved-total line is its own sentence, not the
      //    "unknown" one: "we do not know the total" and "the total we were
      //    given is false" are two different things, and the second one has
      //    something positive to say — how much **has** been listed.
      ? t('progress.withContradictedTotal', {
        archived: view.archived,
        pending: view.pending,
        listed: view.listed,
      })
      : view.percent === null
        ? t('progress.unknownTotal', {
          archived: view.archived,
          pending: view.pending,
          reason: t(view.unknownReasonKey ?? 'progress.reason.noTotalFromApi'),
        })
        : t('progress.withTotal', {
          archived: view.archived,
          total: view.totalKnown,
          percent: view.percent,
          pending: view.pending,
        });
  return head + body;
}
