/**
 * W913 · c4 — the status report carries the tick's own outcome and its build.
 *
 * ## The defect this file pins
 *
 * The archived status record answered "how old is this install's report?"
 * with `reported_at`, and "why is each platform paused?" with a per-platform
 * code — and could not answer "what did this install's *own tick* do before
 * it went quiet?". The four states a person diagnosing a silent install needs
 * told apart — no tab was open, the daily cap was reached, storage was
 * unreadable, idle with nothing owed — were indistinguishable in the archive,
 * because the facts lived only in `storage.local` (`cs_last_backfill_tick`,
 * written by `recordAlarmTick`) and never left the browser. A 15-hour
 * standstill and a quiet, healthy install wrote the same record.
 *
 * The fix puts the tick's outcome on the wire with the report: `tick_ran`,
 * `tick_reason`, `tick_stopped` and `tick_halted` — the same four facts the
 * durable local trace keeps — plus `build_stamp`, so two installs running the
 * same manifest version but different builds can be told apart by the record
 * alone (the stamp is the same identity a halt record carries, W59b).
 *
 * ## What is asserted, and how it is made to fail
 *
 * A real alarm tick is driven through the real entry point (the alarm
 * listener, then `backfillTickSettled()`, as in W62b), at `no-targets` — the
 * ordinary state of a machine whose browser is closed — with a synthetic
 * native host wired into `runtime.sendNativeMessage` so the report that
 * leaves this browser can be read. The tick is the real one because the
 * property under test is a *derivation*: the four fields must say what the
 * local trace says, and asserting that from hand-built inputs would be
 * asserting the mock, not the writer.
 *
 * Three facts are pinned, in the order a reader loses them:
 *
 *  · the wire record carries the tick's outcome at all — before the fix the
 *    four fields are simply absent from the status payload;
 *  · `tick_stopped` falls back to the tick's own reason on a run-less tick,
 *    the same W47 rule the durable trace applies, so "how this ended" is
 *    never blank on a tick that never reached a run;
 *  · every outcome field on the wire equals the local trace's field, read
 *    back through `loadLastTick` — the trace and the archived record are two
 *    copies of one fact, and a writer that let them drift would make the
 *    archive answer a question the install's own storage contradicts.
 *
 * No network, no real browser profile, no real conversation data.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n, TEST_BUILD_STAMP } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import type { TabQueryRow } from '../lib/backfill/tab-port';

const store: Record<string, unknown> = {};
const alarmListeners: Array<(a: any) => void> = [];
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
/** Tabs the sweep would answer — left empty, so the tick is the laptop-closed one. */
const queriedTabs: TabQueryRow[] = [];
/** The host this run's reports went to, so the wire payload can be read back. */
let host: SyntheticHost;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
  alarms: {
    create() {},
    async clear() { return true; },
    async get() { return undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    async query() { return queriedTabs.map((t) => ({ ...t })); },
  },
};

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function trace(): Promise<any> {
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  return await loadLastTick(browserLocalStore());
}

/** Let every microtask and one macrotask drain, so the report's own awaits can finish. */
async function settle(): Promise<void> {
  for (let i = 0; i < 50; i += 1) await Promise.resolve();
  await new Promise((r) => setTimeout(r, 0));
}

/** A concluded `no-targets` tick — the laptop-closed state — driven and settled. */
async function idleTick(): Promise<void> {
  const mod = await bootBackground();
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  await mod.backfillTickSettled();
  await settle();
}

/** The one status request the tick sent, once the report has had its awaits. */
function lastStatus(): Record<string, unknown> {
  const status = host.requests().find((request) => request.type === 'status');
  expect(status, 'the tick reported at all — the report is best-effort but must have run').toBeDefined();
  return status!;
}

describe('W913 c4 · the status report carries the tick outcome and the build stamp', () => {
  beforeEach(async () => {
    for (const k of Object.keys(store)) delete store[k];
    alarmListeners.length = 0;
    runtimeListeners.length = 0;
    queriedTabs.length = 0;
    vi.resetModules();
    host = createSyntheticHost({ up: true });
    fakeBrowser.runtime.sendNativeMessage = host.sendNativeMessage;
    vi.stubGlobal('browser', withI18n(fakeBrowser));
    vi.stubGlobal('chrome', fakeBrowser);
    vi.stubGlobal('defineBackground', (cb: any) => cb);
    // Globals are in place first, so `setBackfillEnabled` writes the switch
    // into this run's store — `tests/setup.ts` installs its own browser
    // before a test's stubs do, and a write against that one is a write
    // against storage this tick never reads.
    const { setBackfillEnabled, resetTickLockForTest } = await import('../lib/backfill/schedule');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setBackfillEnabled(browserLocalStore(), true);
    resetTickLockForTest();
    (globalThis as any).indexedDB = new IDBFactory();
  });

  it('🔴 a tickless browser reports the tick that did nothing, not just its age', async () => {
    await idleTick();

    const local = await trace();
    expect(local, 'the durable trace was written at all').toBeTruthy();
    expect(local.reason).toBe('no-targets');

    const payload = lastStatus().status as Record<string, unknown>;
    // The four fields exist on the wire at all. Before the fix this object
    // carried no tick knowledge — the red was an undefined here.
    expect(payload.tick_ran).toBe(false);
    expect(payload.tick_reason).toBe('no-targets');
    // W47, on the wire too: no run happened, so "how this ended" is the
    // tick's own answer — never blank, never invented.
    expect(payload.tick_stopped).toBe('no-targets');
    // Explicit null: this tick was not halted. The field being *absent* is
    // the older extension's state and is a different fact.
    expect(payload.tick_halted).toBeNull();
    expect(payload.build_stamp).toBe(TEST_BUILD_STAMP);
  });

  it('🔴 the wire outcome is the local trace — the record may not drift from it', async () => {
    await idleTick();

    const local = await trace();
    const payload = lastStatus().status as Record<string, unknown>;
    // Two copies of one fact: what the host archives and what the popup's
    // last-tick line reads are written from the same tick, and a writer that
    // let them drift would make the archive answer "no, that tick ran" while
    // the install's own storage says it did not.
    expect(payload.tick_ran).toBe(local.ran);
    expect(payload.tick_reason).toBe(local.reason);
    expect(payload.tick_stopped).toBe(local.stopped);
    expect(payload.tick_halted).toBe(local.halted);
  });
});
