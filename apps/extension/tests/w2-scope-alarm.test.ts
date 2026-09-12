/**
 * Review fixes on top of W2 (contracts/nativehost-protocol.md §6.3 scope column, §10 retry timer).
 *
 *  1. Scope, not `retryable` alone, decides an item's fate. A host-scope nack
 *     (`config`, `protocol-version`, `stage-unavailable`, `io`) keeps the item
 *     pending even when `retryable` is false; only an item-scope, non-retryable
 *     nack rejects it.
 *  2. The outbox has its own retry alarm, independent of the backfill switch:
 *     it exists while anything is pending (or while the outbox cannot be read)
 *     and is cleared once the outbox is empty.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import type { DeliverResult, NackKind } from '../lib/native-host';

const ITEM_SCOPE: NackKind[] = ['bad-request', 'too-large', 'integrity', 'invalid-bundle'];
const HOST_SCOPE: NackKind[] = ['protocol-version', 'config', 'stage-unavailable', 'io'];

function nack(kind: NackKind, retryable: boolean): DeliverResult {
  return { delivered: false, reason: 'nack', kind, retryable, detail: kind, requestId: 'r', sha256: 's' };
}

beforeEach(() => {
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

// ===========================================================================
// 1 · isItemRejected truth table
// ===========================================================================
describe('scope · isItemRejected', () => {
  it('the scope sets cover the whole nack kind set, with no overlap', async () => {
    const { NACK_KINDS, HOST_SCOPE_KINDS } = await import('../lib/native-host');
    expect([...HOST_SCOPE_KINDS].sort()).toEqual([...HOST_SCOPE].sort());
    expect([...ITEM_SCOPE, ...HOST_SCOPE].sort()).toEqual([...NACK_KINDS].sort());
  });

  it('only an item-scope nack with retryable:false rejects the item', async () => {
    const { isItemRejected } = await import('../lib/native-host');
    for (const kind of ITEM_SCOPE) {
      expect(isItemRejected(nack(kind, false)), `${kind}/false`).toBe(true);
      expect(isItemRejected(nack(kind, true)), `${kind}/true`).toBe(false);
    }
    for (const kind of HOST_SCOPE) {
      expect(isItemRejected(nack(kind, false)), `${kind}/false`).toBe(false);
      expect(isItemRejected(nack(kind, true)), `${kind}/true`).toBe(false);
    }
  });

  it('failures that are not a nack never reject the item, whatever their retryable flag', async () => {
    const { isItemRejected } = await import('../lib/native-host');
    for (const reason of ['no-runtime-api', 'send-failed', 'timeout', 'malformed-response', 'crypto-unavailable'] as const) {
      for (const retryable of [true, false]) {
        const r: DeliverResult = { delivered: false, reason, retryable, requestId: 'r', sha256: 's' };
        expect(isItemRejected(r), `${reason}/${retryable}`).toBe(false);
      }
    }
    const ok: DeliverResult = { delivered: true, status: 'stored', shard: 'x', requestId: 'r', sha256: 's' };
    expect(isItemRejected(ok)).toBe(false);
  });
});

// ===========================================================================
// 2 · the drain obeys the scope
// ===========================================================================
const NAME_A = 'chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json';
const PAYLOAD_A = '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}';
const NAME_B = 'chatgpt-bbbbbbbb-1111-2222-3333-444444444444.json';
const PAYLOAD_B = '{"sessionId":"bbbbbbbb-1111-2222-3333-444444444444"}';

describe('scope · drainOutbox', () => {
  it('🔴 host-scope nack `config` (retryable:false) ⇒ nothing rejected, every item still pending, drain stops', async () => {
    const ob = await import('../lib/outbox');
    await ob.enqueue(NAME_A, PAYLOAD_A);
    await ob.enqueue(NAME_B, PAYLOAD_B);
    const deliver = vi.fn(async () => nack('config', false));

    const report = await ob.drainOutbox({ deliver });

    expect(report).toMatchObject({ attempted: 1, rejected: 0, delivered: 0, stoppedBy: 'host-unavailable', lastKind: 'config' });
    expect(deliver).toHaveBeenCalledTimes(1);
    const entries = (await ob.listEntries())!;
    expect(entries.map((e) => e.state)).toEqual(['pending', 'pending']);
    // Which of the two went first depends on the FIFO tie-break (same enqueue
    // millisecond ⇒ ordered by sha), so assert the shape, not the name.
    const tried = entries.filter((e) => e.attempts === 1);
    const untouched = entries.filter((e) => e.attempts === 0);
    expect(tried).toHaveLength(1);
    expect(tried[0]).toMatchObject({ lastError: 'nack:config' });
    expect(untouched).toHaveLength(1);
    expect(entries.every((e) => e.rejectKind === undefined)).toBe(true);
  });

  it('every host-scope kind keeps the item pending', async () => {
    for (const kind of HOST_SCOPE) {
      vi.resetModules();
      (globalThis as any).indexedDB = new IDBFactory();
      const ob = await import('../lib/outbox');
      await ob.enqueue(NAME_A, PAYLOAD_A);
      await ob.drainOutbox({ deliver: async () => nack(kind, false) });
      expect((await ob.listEntries())![0]!.state, kind).toBe('pending');
    }
  });

  it('a missing runtime API (retryable:false, not a nack) keeps the item pending', async () => {
    const ob = await import('../lib/outbox');
    await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({
      deliver: async () => ({ delivered: false, reason: 'no-runtime-api', retryable: false, requestId: '', sha256: '' }),
    });
    expect(report.rejected).toBe(0);
    expect((await ob.listEntries())![0]!.state).toBe('pending');
  });

  it('an item-scope nack still rejects that item and the drain moves on to the next', async () => {
    const ob = await import('../lib/outbox');
    await ob.enqueue(NAME_A, PAYLOAD_A);
    await ob.enqueue(NAME_B, PAYLOAD_B);
    const deliver = vi.fn(async (name: string): Promise<DeliverResult> => (name === NAME_A
      ? nack('invalid-bundle', false)
      : { delivered: true, status: 'stored', shard: 's1', requestId: 'r', sha256: 's' }));

    const report = await ob.drainOutbox({ deliver });

    expect(report).toMatchObject({ attempted: 2, rejected: 1, delivered: 1, stoppedBy: 'drained' });
    const entries = (await ob.listEntries())!;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ name: NAME_A, state: 'rejected', rejectKind: 'invalid-bundle' });
  });
});

// ===========================================================================
// 3 · syncOutboxAlarm
// ===========================================================================
function fakeAlarms() {
  const live = new Map<string, { periodInMinutes?: number }>();
  const calls: string[] = [];
  return {
    live,
    calls,
    api: {
      create(name: string, info: { periodInMinutes?: number }) { calls.push(`create:${name}`); live.set(name, info); },
      clear(name: string) { calls.push(`clear:${name}`); return live.delete(name); },
      async get(name: string) { return live.get(name); },
    },
  };
}

describe('outbox alarm · syncOutboxAlarm', () => {
  it('pending > 0 ⇒ creates the alarm with the documented name and period', async () => {
    const { syncOutboxAlarm, OUTBOX_ALARM_NAME, OUTBOX_ALARM_PERIOD_MINUTES } = await import('../lib/outbox-alarm');
    const a = fakeAlarms();
    expect(await syncOutboxAlarm(a.api, 3)).toBe('created');
    expect(a.live.get(OUTBOX_ALARM_NAME)).toEqual({ periodInMinutes: OUTBOX_ALARM_PERIOD_MINUTES });
  });

  it('an existing alarm is kept, not re-created (its period is not restarted)', async () => {
    const { syncOutboxAlarm } = await import('../lib/outbox-alarm');
    const a = fakeAlarms();
    await syncOutboxAlarm(a.api, 1);
    expect(await syncOutboxAlarm(a.api, 2)).toBe('kept');
    expect(a.calls.filter((c) => c.startsWith('create:'))).toHaveLength(1);
  });

  it('pending = 0 ⇒ clears it', async () => {
    const { syncOutboxAlarm, OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    const a = fakeAlarms();
    await syncOutboxAlarm(a.api, 1);
    expect(await syncOutboxAlarm(a.api, 0)).toBe('cleared');
    expect(a.live.has(OUTBOX_ALARM_NAME)).toBe(false);
  });

  it('🔴 pending unknown (outbox unreadable) ⇒ keeps an alarm: unknown is never empty', async () => {
    const { syncOutboxAlarm, OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    const a = fakeAlarms();
    expect(await syncOutboxAlarm(a.api, null)).toBe('created');
    expect(a.live.has(OUTBOX_ALARM_NAME)).toBe(true);
  });

  it('no alarms API ⇒ says unavailable, never pretends', async () => {
    const { syncOutboxAlarm } = await import('../lib/outbox-alarm');
    expect(await syncOutboxAlarm(null, 3)).toBe('unavailable');
  });

  it('the name differs from the backfill alarm, so the two lifecycles cannot clear each other', async () => {
    const { OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    expect(OUTBOX_ALARM_NAME).not.toBe(BACKFILL_ALARM_NAME);
  });
});

// ===========================================================================
// 4 · end to end through the real background entry point
// ===========================================================================
describe('outbox alarm · background wiring (backfill switch OFF)', () => {
  const store: Record<string, unknown> = {};
  const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
  let alarmListeners: Array<(a: { name?: string }) => void> = [];
  let hostMode: 'up' | 'down' | 'nack-config' = 'up';
  let deliveries: string[] = [];
  const SID = 'aaaaaaaa-1111-2222-3333-444444444444';
  let alarms = fakeAlarms();

  function makeBrowser(): any {
    return {
      runtime: {
        id: 'w2-scope-alarm',
        onStartup: { addListener() {} },
        onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
        async sendNativeMessage(_host: string, message: Record<string, any>) {
          if (hostMode === 'down') throw new Error('Specified native messaging host not found.');
          if (hostMode === 'nack-config') {
            return {
              protocol: 1, type: 'nack', request_id: message.request_id ?? null, kind: 'config', retryable: false,
              detail: 'run: chat-stasher install-native-host --stage <path>',
            };
          }
          if (message.type === 'hello') {
            return { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: '/stage' };
          }
          deliveries.push(message.name);
          return { protocol: 1, type: 'ack', request_id: message.request_id, status: 'stored', sha256: message.sha256, shard: 's' };
        },
      },
      storage: {
        local: {
          async get(query: Record<string, unknown> | null) {
            if (query === null) return { ...store };
            const out: Record<string, unknown> = {};
            for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
            return out;
          },
          async set(values: Record<string, unknown>) { Object.assign(store, values); },
          async remove(keys: string[]) { for (const k of keys) delete store[k]; },
        },
        onChanged: { addListener() {} },
      },
      action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
      alarms: { ...alarms.api, onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } } },
    };
  }

  function capture(): CapturedFetch {
    return {
      url: `https://chatgpt.com/backend-api/conversation/${SID}`,
      method: 'GET',
      status: 200,
      text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-1' }),
      pageUrl: `https://chatgpt.com/c/${SID}`,
      capturedAt: 1_700_000_000_001,
    };
  }

  async function boot(): Promise<any> {
    const mod: any = await import('../entrypoints/background');
    if (runtimeListeners.length === 0) await mod.default();
    return mod;
  }

  async function dispatch(mod: any): Promise<any> {
    const replied = await new Promise<any>((resolve) => {
      runtimeListeners[0]!({ type: 'chat-captured', payload: capture() }, { id: 's' }, resolve);
    });
    await mod.backfillTickSettled();
    return replied;
  }

  beforeEach(() => {
    for (const k of Object.keys(store)) delete store[k];
    runtimeListeners.length = 0;
    alarmListeners = [];
    deliveries = [];
    alarms = fakeAlarms();
    const b = withI18n(makeBrowser());
    vi.stubGlobal('browser', b);
    vi.stubGlobal('chrome', b);
    vi.stubGlobal('defineBackground', (cb: any) => cb);
  });

  it('🔴 host down ⇒ capture queued and the outbox alarm exists even though backfill is off; host back ⇒ the alarm alone delivers it and then clears itself', async () => {
    const { OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    const { BACKFILL_ALARM_NAME } = await import('../lib/backfill/alarm');
    hostMode = 'down';
    const mod = await boot();
    const result = await dispatch(mod);

    expect(result).toMatchObject({ saved: false, status: 'queued' });
    expect(alarms.live.has(BACKFILL_ALARM_NAME)).toBe(false); // the switch is off
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(true);

    hostMode = 'up';
    const realNow = Date.now();
    vi.spyOn(Date, 'now').mockReturnValue(realNow + 10 * 60_000); // past the 1-minute backoff
    for (const fn of alarmListeners) fn({ name: OUTBOX_ALARM_NAME });
    await mod.outboxAlarmSettled();

    expect(deliveries).toEqual([`chatgpt-${SID}.json`]);
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(false);
  });

  it('🔴 live leg: a `config` nack leaves the capture queued (not rejected), with the alarm armed', async () => {
    const { OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    hostMode = 'nack-config';
    const mod = await boot();
    const result = await dispatch(mod);

    expect(result).toMatchObject({ saved: false, status: 'queued' });
    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ state: 'pending', lastError: 'nack:config' });
    expect(entries[0]!.rejectKind).toBeUndefined();
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(true);
  });

  it('a context with no IndexedDB API at all creates no outbox alarm (nothing can ever be queued there)', async () => {
    const { OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    delete (globalThis as any).indexedDB;
    await boot();
    expect(alarms.live.has(OUTBOX_ALARM_NAME)).toBe(false);
    expect(alarms.calls.filter((c) => c === `create:${OUTBOX_ALARM_NAME}`)).toEqual([]);
  });

  it('an alarm tick that is not the outbox alarm does not drain the outbox', async () => {
    hostMode = 'down';
    const mod = await boot();
    await dispatch(mod);
    hostMode = 'up';
    vi.spyOn(Date, 'now').mockReturnValue(Date.now() + 10 * 60_000);
    for (const fn of alarmListeners) fn({ name: 'some-other-alarm' });
    await mod.outboxAlarmSettled();
    expect(deliveries).toEqual([]);
  });
});
