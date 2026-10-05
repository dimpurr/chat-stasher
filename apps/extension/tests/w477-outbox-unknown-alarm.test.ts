import { describe, expect, it } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import {
  OUTBOX_ALARM_NAME,
  OUTBOX_ALARM_PERIOD_MINUTES,
  syncOutboxAlarmFromRead,
} from '../lib/outbox-alarm';
import { OUTBOX_CAPACITY_BYTES, isOutboxSummary } from '../lib/outbox';

const NAME_A = 'chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json';
const NAME_B = 'chatgpt-bbbbbbbb-1111-2222-3333-444444444444.json';
const PAYLOAD_A = '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}';
const PAYLOAD_B = '{"sessionId":"bbbbbbbb-1111-2222-3333-444444444444"}';

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

/** A summary with the outbox's own default capacity and nothing queued. */
function emptySummary() {
  return {
    pending: 0,
    rejected: 0,
    bytes: 0,
    capacityBytes: OUTBOX_CAPACITY_BYTES,
    full: false,
    nearFull: false,
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

    expect(await syncOutboxAlarmFromRead(alarms.api, true, async () => emptySummary())).toBe('cleared');
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

  it('arms the alarm for a measured non-empty outbox, so unknown is not the only way to get one', async () => {
    const alarms = fakeAlarms();

    expect(await syncOutboxAlarmFromRead(alarms.api, true, async () => ({ ...emptySummary(), pending: 2, bytes: 64 })))
      .toBe('created');
    expect(alarms.live.get(OUTBOX_ALARM_NAME)).toEqual({ periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });
  });
});

/**
 * 🔴 The guard the whole rule rests on. `isOutboxSummary` re-derives `summary()`'s
 *    own arithmetic, so these cases pin that the two stay in step: a summary this
 *    module *would* have written is a measurement, and anything else is unknown.
 *    The accepted values are the ones `summary()` returns for a queue that is
 *    empty, part-occupied, and full — not hand-picked shapes.
 */
describe('W477 · isOutboxSummary · what the outbox itself could have written', () => {
  it.each([
    ['an empty queue', emptySummary()],
    ['a partly occupied queue', { ...emptySummary(), pending: 3, rejected: 1, bytes: 4096 }],
    ['a near-full queue', {
      ...emptySummary(), pending: 1, bytes: Math.ceil(OUTBOX_CAPACITY_BYTES * 0.8), nearFull: true,
    }],
    ['a full queue', {
      ...emptySummary(), pending: 1, bytes: OUTBOX_CAPACITY_BYTES, full: true, nearFull: true,
    }],
  ])('%s is a measurement', (_case, value) => {
    expect(isOutboxSummary(value)).toBe(true);
  });

  it.each([
    ['null', null],
    ['undefined', undefined],
    ['a string', 'pending'],
    ['a number', 0],
    ['an array', []],
    ['a zero-capacity summary', { ...emptySummary(), capacityBytes: 0 }],
    ['a negative count', { ...emptySummary(), rejected: -1 }],
    ['a fractional count', { ...emptySummary(), pending: 0.5 }],
    ['a non-finite count', { ...emptySummary(), bytes: Number.NaN }],
    ['a count above the safe-integer range', { ...emptySummary(), bytes: Number.MAX_SAFE_INTEGER + 2 }],
    ['a summary with no fields', {}],
    ['a count spelled as a string', { ...emptySummary(), pending: '0' }],
    ['a full flag that contradicts the bytes', { ...emptySummary(), bytes: OUTBOX_CAPACITY_BYTES, full: false, nearFull: false }],
    ['a nearFull flag below the threshold', { ...emptySummary(), bytes: 1, nearFull: true }],
    ['a nearFull flag one byte under the threshold', {
      ...emptySummary(), pending: 1, bytes: Math.floor(OUTBOX_CAPACITY_BYTES * 0.8), nearFull: true,
    }],
  ])('%s is not a summary, so a caller must read it as unknown', (_case, value) => {
    expect(isOutboxSummary(value)).toBe(false);
  });
});

/**
 * 🔴 The guard accepts every summary this module writes. `enqueue` reports a
 *    refusal with a summary of its own, and the guard is what decides
 *    measured-vs-unknown for whoever reads one next — so a writer that produces a
 *    value its own guard refuses is a trap for the next caller, not a formality.
 *
 *    The case that matters is a refusal with room still left: the newcomer did not
 *    fit, but the queue is not at capacity. `full` means "nothing more can be
 *    accepted" and is `bytes >= capacityBytes` everywhere else in the module, so it
 *    is derived here too rather than asserted from the refusal. The refusal itself
 *    is still reported, in `reason`.
 */
describe('W477 · the outbox’s own refusal summary is a summary its guard accepts', () => {
  it('🔴 refuses the newcomer with room to spare, and the summary beside it is still a measurement', async () => {
    (globalThis as any).indexedDB = new IDBFactory();
    const ob = await import('../lib/outbox');

    expect((await ob.enqueue(NAME_A, PAYLOAD_A, { capacityBytes: 10_000 })).accepted).toBe(true);
    const used = (await ob.summary({ capacityBytes: 10_000 }))!.bytes;

    // One byte of room against a payload that needs far more: refused, yet `bytes`
    // is still under capacity, which is exactly where a hard-coded `full: true`
    // contradicted the guard's own derivation.
    const refused = await ob.enqueue(NAME_B, PAYLOAD_B, { capacityBytes: used + 1 });
    expect(refused.accepted).toBe(false);
    expect(refused.reason).toBe('outbox-full');
    expect(refused.summary).toMatchObject({ bytes: used, capacityBytes: used + 1, full: false });
    expect(ob.isOutboxSummary(refused.summary)).toBe(true);
  });

  it('still reports `full` when the queue really is at capacity', async () => {
    (globalThis as any).indexedDB = new IDBFactory();
    const ob = await import('../lib/outbox');

    expect((await ob.enqueue(NAME_A, PAYLOAD_A, { capacityBytes: 10_000 })).accepted).toBe(true);
    const used = (await ob.summary({ capacityBytes: 10_000 }))!.bytes;

    const refused = await ob.enqueue(NAME_B, PAYLOAD_B, { capacityBytes: used });
    expect(refused.reason).toBe('outbox-full');
    expect(refused.summary).toMatchObject({ full: true });
    expect(ob.isOutboxSummary(refused.summary)).toBe(true);
  });
});
