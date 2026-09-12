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
 *  · pending unknown (the outbox could not be read) ⇒ keep an alarm.
 *    🔴 Unknown is never treated as empty: a timer that fires on an empty
 *    outbox costs one read; a missing timer on a full one loses retries.
 */

import type { AlarmsApi, AlarmSyncResult } from './backfill/alarm';

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
