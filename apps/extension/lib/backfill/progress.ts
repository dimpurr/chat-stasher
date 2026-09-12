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
 *      untrustworthy, and we go back to unknown).
 * If any one of those fails we take the "total unknown" wording, and that
 * output contains **no '%' character at all**.
 *
 * Why "enumerated so far" is not allowed to be the denominator: while
 * enumeration is still running it keeps growing, so the bar would shoot up to
 * 100% and fall back down — a fake percentage, worse than no progress bar.
 *
 * The wording lives in the catalog; this file only decides *which* sentence and
 * with what numbers. `unknownReasonKey` is a message key, not a sentence, so
 * that a progress view computed once is still renderable in either language.
 */

import { t } from '../i18n';
import type { BackfillState } from './types';

export interface ProgressView {
  archived: number;
  pending: number;
  /** Only a number when the denominator is trustworthy, otherwise null. */
  totalKnown: number | null;
  /** Integer 0-100; null when untrustworthy. null ⇒ the renderer must not emit '%'. */
  percent: number | null;
  /**
   * Catalog key for why there is no percentage (null when there is one) — so
   * that "we do not know" is itself readable, in the reader's language.
   */
  unknownReasonKey: string | null;
  halted: BackfillState['halted'];
}

export function computeProgress(state: BackfillState): ProgressView {
  const archived = state.archived.length;
  const pending = state.pending.length;

  let percent: number | null = null;
  let unknownReasonKey: string | null = null;
  let totalKnown: number | null = null;

  if (state.totalSource !== 'response-total') {
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

  return { archived, pending, totalKnown, percent, unknownReasonKey, halted: state.halted };
}

/**
 * One line of plain-language progress.
 * With an unknown denominator it reads like: `Archived 12, still owed 30, total
 * unknown (the API did not provide a total)` — say how much is done, how much
 * is still owed, and why there is no denominator. What it does not do is
 * produce a percentage.
 */
export function formatProgress(state: BackfillState): string {
  const view = computeProgress(state);
  const head = view.halted ? t('progress.haltedPrefix', { reason: view.halted.reason }) : '';
  const body =
    view.percent === null
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
