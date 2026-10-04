import { describe, expect, it } from 'vitest';
import {
  OUTBOX_ALARM_NAME,
  OUTBOX_ALARM_PERIOD_MINUTES,
  syncOutboxAlarmFromRead,
} from '../lib/outbox-alarm';

function fakeAlarms() {
  const live = new Map<string, { periodInMinutes?: number }>();
  const calls: string[] = [];
  return {
    live,
    calls,
    api: {
      create(name: string, info: { periodInMinutes?: number }) {
        calls.push(`create:${name}`);
        live.set(name, info);
      },
      async clear(name: string) {
        calls.push(`clear:${name}`);
        return live.delete(name);
      },
      async get(name: string) { return live.get(name); },
    },
  };
}

describe('W477 · unknown outbox reads keep the retry alarm scheduled', () => {
  it.each([
    ['summary reader returns null', async () => null],
    ['read throws', async () => { throw new Error('synthetic read failure'); }],
    ['malformed summary', async () => ({ pending: '0' })],
    ['missing pending count', async () => ({})],
    ['malformed sibling field', async () => ({ pending: 0, rejected: '0', bytes: 0, capacityBytes: 1, full: false, nearFull: false })],
    ['inconsistent sibling fields', async () => ({ pending: 0, rejected: 0, bytes: 1, capacityBytes: 1, full: false, nearFull: false })],
  ])('%s is unknown, never a verified empty outbox', async (_case, read) => {
    const alarms = fakeAlarms();

    expect(await syncOutboxAlarmFromRead(alarms.api, true, read)).toBe('created');
    expect(alarms.live.get(OUTBOX_ALARM_NAME)).toEqual({ periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });
    expect(alarms.calls).toEqual([`create:${OUTBOX_ALARM_NAME}`]);
  });

  it('keeps an existing alarm when the read is unknown', async () => {
    const alarms = fakeAlarms();
    alarms.live.set(OUTBOX_ALARM_NAME, { periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });

    expect(await syncOutboxAlarmFromRead(alarms.api, true, async () => { throw new Error('synthetic failure'); }))
      .toBe('kept');
    expect(alarms.calls).toEqual([]);
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(true);
  });

  it('clears the alarm for a verified empty outbox', async () => {
    const alarms = fakeAlarms();
    alarms.live.set(OUTBOX_ALARM_NAME, { periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });

    expect(await syncOutboxAlarmFromRead(alarms.api, true, async () => ({
      pending: 0,
      rejected: 0,
      bytes: 0,
      capacityBytes: 256 * 1024 * 1024,
      full: false,
      nearFull: false,
    }))).toBe('cleared');
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(false);
    expect(alarms.calls).toEqual([`clear:${OUTBOX_ALARM_NAME}`]);
  });

  it('treats a context without IndexedDB as verified empty, distinct from an unreadable store', async () => {
    const alarms = fakeAlarms();
    alarms.live.set(OUTBOX_ALARM_NAME, { periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });
    const read = async () => {
      throw new Error('must not read when storage API is absent');
    };

    expect(await syncOutboxAlarmFromRead(alarms.api, false, read)).toBe('cleared');
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(false);
    expect(alarms.calls).toEqual([`clear:${OUTBOX_ALARM_NAME}`]);
  });
});
