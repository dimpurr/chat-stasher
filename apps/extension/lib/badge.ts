/**
 * The toolbar badge. One source of truth: the outbox.
 *
 * What it says now (this replaced the old "captures in the last 5 minutes"
 * counter, which belonged to the automatic-download channel):
 *
 *   • nothing waiting              → no badge
 *   • N captures waiting           → the number N
 *   • something rejected, or the   → the alert mark, with a title that spells out
 *     outbox is full                 how many are waiting, how many were
 *                                    rejected, and whether it is full
 *
 * 🔴 Why the alert outranks the number: a number means "work in progress, the
 *    host will take these". A rejection or a full outbox means *nothing will
 *    move without you*; showing "3" for that case would read as normality.
 *
 * The old staleness rule is gone with the counter it guarded. It existed because
 * a count kept in `storage.local` could outlive the worker that wrote it; the
 * outbox is the durable record and the badge is now derived from it, so a badge
 * that disagrees with the outbox is impossible by construction rather than
 * corrected five minutes later.
 */

import { summary, type OutboxSummary, type OutboxOptions } from './outbox';
import { badgeTitle, badgeUnreadable } from './ui-strings';

/** Number-of-waiting background. */
export const BADGE_COLOR_WAITING = '#c1353c';
/** Something needs the user: rejected entries or a full outbox. */
export const BADGE_COLOR_ALERT = '#8b0000';

/** The badge mark used for the alert state. Not a digit, so it cannot be read as a count. */
export const BADGE_ALERT_TEXT = '!';

export interface BadgePlan {
  text: string;
  color: string;
  title: string;
}

/**
 * Pure: outbox summary → what the badge should look like.
 * `null` means "the badge must be empty" (an unambiguous off signal; a grey
 * badge renders differently across themes and can still read as "running").
 */
export function badgeFor(state: OutboxSummary | null): BadgePlan | null {
  if (state === null) {
    // 🔴 "the outbox cannot be read" is not "nothing is waiting". The two must
    //    not look alike on the badge either: a number or an empty badge both say
    //    "I know the state", and here we genuinely do not.
    return {
      text: BADGE_ALERT_TEXT,
      color: BADGE_COLOR_ALERT,
      title: badgeUnreadable(),
    };
  }
  const alert = state.rejected > 0 || state.full;
  if (alert) {
    return {
      text: BADGE_ALERT_TEXT,
      color: BADGE_COLOR_ALERT,
      title: badgeTitle(state.pending, state.rejected, state.full),
    };
  }
  if (state.pending > 0) {
    return {
      text: String(state.pending),
      color: BADGE_COLOR_WAITING,
      title: badgeTitle(state.pending, 0, false),
    };
  }
  return null;
}

type ActionApi = {
  setBadgeText: (o: { text: string }) => Promise<void>;
  setBadgeBackgroundColor?: (o: { color: string }) => Promise<void>;
  setTitle?: (o: { title: string }) => Promise<void>;
};

function actionApi(): ActionApi | null {
  // Chrome exposes the extension APIs on `chrome`; only Firefox defines a
  // global `browser`. Looking at `browser` alone left the badge unpainted in
  // every Chromium browser while every unit test (which stubs `browser`) stayed
  // green — found by the real-browser E2E run.
  const g = globalThis as { browser?: { action?: ActionApi }; chrome?: { action?: ActionApi } };
  const action = g.browser?.action ?? g.chrome?.action;
  return action && typeof action.setBadgeText === 'function' ? action : null;
}

/** Cosmetic: a missing action surface is never fatal. */
export async function clearBadge(): Promise<void> {
  const action = actionApi();
  if (!action) return;
  await action.setBadgeText({ text: '' });
}

async function paint(plan: BadgePlan | null): Promise<void> {
  const action = actionApi();
  if (!action) return;
  if (plan === null) {
    await action.setBadgeText({ text: '' });
    return;
  }
  await action.setBadgeText({ text: plan.text });
  await action.setBadgeBackgroundColor?.({ color: plan.color });
  await action.setTitle?.({ title: plan.title });
}

/**
 * Re-derive the badge from the outbox. Safe to call on every worker wake and
 * after every drain: it is a read plus a paint, and it never throws.
 */
export async function refreshBadge(options: OutboxOptions = {}): Promise<BadgePlan | null> {
  try {
    const plan = badgeFor(await summary(options));
    await paint(plan);
    return plan;
  } catch (err) {
    console.warn('[chat-stasher] badge update failed', (err as Error).message);
    return null;
  }
}
