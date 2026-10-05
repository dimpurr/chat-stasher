/**
 * The outbox's own retry timer (`contracts/nativehost-protocol.md` §10).
 *
 * Why it is separate from the backfill alarm: that alarm exists only while the
 * backfill switch is on (C19). With backfill off, a live capture queued while
 * the host was down would otherwise wait for the *next* capture before anyone
 * tried again — an unbounded delay nobody chose.
 *
 * Rule:
 *  · pending > 0  ⇒ an alarm exists (an existing one is kept, so its period is
 *    not restarted on every service-worker wake);
 *  · pending = 0  ⇒ no alarm;
 *  · pending unknown (the outbox could not be read, or what came back is not a
 *    summary this build wrote) ⇒ keep an alarm.
 *    🔴 Unknown is never treated as empty: a timer that fires on an empty
 *    outbox costs one read; a missing timer on a full one loses retries.
 */

import type { AlarmsApi, AlarmSyncResult } from './backfill/alarm';
import { isOutboxSummary } from './outbox';

export const OUTBOX_ALARM_NAME = 'cs-outbox-retry';

/**
 * Every 5 minutes. Entries still inside their backoff window are skipped by
 * the drain, so a tick on a waiting outbox is one IndexedDB read and nothing
 * else.
 */
export const OUTBOX_ALARM_PERIOD_MINUTES = 5;

export async function syncOutboxAlarm(
  alarms: AlarmsApi | null | undefined,
  pending: number | null,
): Promise<AlarmSyncResult> {
  if (!alarms || typeof alarms.create !== 'function' || typeof alarms.clear !== 'function') {
    return 'unavailable';
  }
  if (pending === 0) {
    await alarms.clear(OUTBOX_ALARM_NAME);
    return 'cleared';
  }
  if (typeof alarms.get === 'function') {
    const existing = await alarms.get(OUTBOX_ALARM_NAME);
    if (existing) return 'kept';
  }
  await alarms.create(OUTBOX_ALARM_NAME, { periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });
  return 'created';
}

/**
 * Read the outbox state for alarm scheduling, keeping this module's rule (see
 * the file header) true of every way the read can come back.
 *
 * 🔴 The three states stay three states. A missing IndexedDB API means the
 *    outbox cannot exist in this context, so absence there *is* measured and the
 *    queue is empty. Once the API exists, a read that throws, returns `null`, or
 *    returns something `isOutboxSummary` refuses is **unknown** — never zero —
 *    and `syncOutboxAlarm(null)` keeps the timer armed. A timer that fires on
 *    an empty outbox costs one IndexedDB read; a missing timer on a full one
 *    loses retries with nothing to say so.
 *
 * 🔴 The reader is injected rather than imported so this rule is exercised
 *    against every shape the read can produce, including the ones no IndexedDB
 *    stub in the tree returns.
 */
export async function syncOutboxAlarmFromRead(
  alarms: AlarmsApi | null | undefined,
  storageAvailable: boolean,
  readSummary: () => Promise<unknown>,
): Promise<AlarmSyncResult> {
  if (!storageAvailable) return syncOutboxAlarm(alarms, 0);

  let pending: number | null = null;
  try {
    const summary = await readSummary();
    if (isOutboxSummary(summary)) pending = summary.pending;
    else console.warn('[chat-stasher] outbox summary is unreadable or malformed; keeping retry alarm scheduled');
  } catch (err) {
    // The API exists, so an unreadable queue is not evidence that it is empty.
    console.warn('[chat-stasher] outbox summary read failed; keeping retry alarm scheduled', (err as Error).message);
  }
  return syncOutboxAlarm(alarms, pending);
}
