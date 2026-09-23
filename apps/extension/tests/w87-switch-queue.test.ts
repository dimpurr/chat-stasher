// @vitest-environment jsdom
/**
 * W87 · **The popup's backfill toggle is a caller of the service worker's alarm
 * queue, not a second alarm writer beside it.**
 *
 * ## The defect this file exists for
 *
 * W82 gave background one serial chain for switch-driven alarm syncs
 * (`alarmSyncQueue`, `entrypoints/background.ts`) so that "the first read of the
 * switch can be the last write to an alarm" could not happen. It left exactly one
 * writer outside that chain: `onToggle` in the popup wrote the switch and then
 * called `syncBackfillAlarm` **in the popup's own realm**. That call had no
 * ordering relationship with the chain at all, so whichever of the two settled
 * last won — and the popup's could settle last in both directions:
 *
 *   · **off** — an on-sync in the popup realm is in flight (it read `true` and is
 *     inside `alarms.get`). The user turns the box off. The chain, queued by the
 *     write's `storage.onChanged`, reads `false` and clears both alarms; the
 *     popup's earlier call then finishes the on path and re-creates them. The
 *     switch is off and the alarms are armed. Ticks refuse at the `disabled` gate
 *     so no request is made, but the worker keeps waking for the rest of the day.
 *   · **on** — an off-sync in the popup realm is awaiting `alarms.clear`. The user
 *     turns the box back on. The chain reads `true` and arms both alarms; the
 *     popup's clear then lands and deletes them. Switch on, nothing armed, and
 *     nothing re-arms it until some unrelated wake. That is precisely the "the
 *     chain can never stay broken" property W16 exists for.
 *
 * ## What is asserted, and why each one is a different claim
 *
 *  1. **The popup performs no alarm operation.** Driven through the popup's own
 *     module and its own markup, with `browser.alarms` recording every call —
 *     and with no background module in the process, so every recorded call is
 *     the popup's. It must write the switch and ask the worker instead.
 *  2. **off during an in-flight popup "on" sync ends off and unarmed.** The
 *     interleaving above, forced rather than raced.
 *  3. **on during an in-flight popup "off" sync ends on and armed.** The same,
 *     in the other direction.
 *  4. **A toggle still takes effect promptly**, with the worker booted and no
 *     other event: the alarm appears when the box goes on and is gone when it
 *     goes off. This one is a **guard, not a regression test** — it passes
 *     against the unfixed code too, because the unfixed popup armed the alarms
 *     itself. Its point is that the fix does not cost the user the effect.
 *  5. **A worker that does not answer is `unknown`, never a value.** The reply is
 *     a log line; "this popup did not learn the outcome" and "the alarms were
 *     cleared" must stay two different states.
 *
 * 🔴 2 and 3 are forced with a one-shot gate on the alarm call the popup is in
 *    flight in, armed **while no background module exists in this process**. That
 *    is the real shape of the race — the popup's call was issued first and
 *    settles last — and it makes the interleaving a fact rather than a hope. A
 *    test that merely raced the two would pass on the unfixed code whenever the
 *    shorter path happened to win, which is the same trap W82's own test names.
 *
 * Zero network and zero logged-in state throughout: a fake `browser.*` around the
 * shared synthetic native host (tests/synthetic-native-host.ts).
 */

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { createSyntheticHost } from './synthetic-native-host';

const POPUP_HTML = readFileSync(
  resolve(__dirname, '..', 'entrypoints', 'popup', 'index.html'),
  'utf8',
);

/** Put the popup's real body into the document — the same markup the browser loads. */
function mountPopup(): void {
  const start = POPUP_HTML.indexOf('<body>') + '<body>'.length;
  const body = POPUP_HTML.slice(start, POPUP_HTML.lastIndexOf('</body>'));
  document.body.innerHTML = body.replace(/<script[\s\S]*?<\/script>/g, '');
}

// --------------------------------------------------------------------------
// The shared fake: storage, alarms, runtime, tabs.
// --------------------------------------------------------------------------

/** `storage.local`'s contents. One object for the whole file, cleared per test. */
const store: Record<string, unknown> = {};
const changeListeners: Array<
  (changes: Record<string, { oldValue?: unknown; newValue?: unknown }>, area: string) => void
> = [];

const alarmBook = new Map<string, unknown>();
/** Every alarm call, in order — including the ones the popup must not make. */
const alarmOps: string[] = [];
const alarmListeners: Array<(alarm: { name?: string }) => void> = [];

const runtimeListeners: Array<
  (message: any, sender: any, respond: (reply: unknown) => void) => unknown
> = [];
/** Every message the popup (or anything else) sent through `runtime.sendMessage`. */
const sentMessages: Array<Record<string, unknown>> = [];

let host: ReturnType<typeof createSyntheticHost>;

/** The two alarms the switch governs — the gate never holds anything else. */
const BACKFILL_ALARMS = ['cs-backfill-tick', 'cs-backfill-safety'];

/**
 * A **one-shot** hold on the next backfill-alarm call named by `armGate(op)`.
 *
 * One-shot, not a lock: the point is that this call was issued *earlier* and
 * settles *later*, which is the whole race. Holding every subsequent call as well
 * would change the ordering being tested rather than reproduce it.
 *
 * Scoped to `get` or `clear` **and** to the two backfill alarms: the outbox has
 * its own alarm and its own sync (`cs-outbox-retry`), and a gate that caught that
 * one would be holding a call this file is not about.
 */
let gateOp: string | null = null;
let gatePromise: Promise<void> | null = null;
let releaseGate: (() => void) | null = null;

function armGate(op: 'get' | 'clear'): void {
  gateOp = op;
  gatePromise = new Promise<void>((resolve) => {
    releaseGate = resolve;
  });
}

function letGateThrough(): void {
  gateOp = null;
  gatePromise = null;
  const release = releaseGate;
  releaseGate = null;
  release?.();
}

async function throughGate(op: string, name: string): Promise<void> {
  if (gateOp !== op) return;
  if (!BACKFILL_ALARMS.includes(name)) return;
  const parked = gatePromise!;
  gateOp = null; // one-shot: only the first matching call parks
  await parked;
}

function memoryArea() {
  const changesOf = (before: Record<string, unknown>, after: Record<string, unknown>) => {
    const changes: Record<string, { oldValue?: unknown; newValue?: unknown }> = {};
    for (const key of Object.keys(after)) {
      if (before[key] === after[key]) continue; // a no-op write is not announced
      changes[key] = { oldValue: before[key], newValue: after[key] };
    }
    return changes;
  };
  const announce = (changes: Record<string, { oldValue?: unknown; newValue?: unknown }>) => {
    if (Object.keys(changes).length === 0) return;
    for (const fn of changeListeners) fn(changes, 'local');
  };
  return {
    async get(query: Record<string, unknown> | null) {
      if (query === null) return { ...store };
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(query)) out[key] = key in store ? store[key] : query[key];
      return out;
    },
    async set(values: Record<string, unknown>) {
      const before = { ...store };
      Object.assign(store, values);
      announce(changesOf(before, values));
    },
    async remove(keys: string | string[]) {
      const before = { ...store };
      const after: Record<string, unknown> = {};
      for (const key of ([] as string[]).concat(keys)) {
        if (key in store) after[key] = undefined;
        delete store[key];
      }
      announce(changesOf(before, after));
    },
  };
}

function fakeBrowser(): any {
  return {
    runtime: {
      id: 'w87-switch-queue-test',
      lastError: undefined,
      onStartup: { addListener() {} },
      onMessage: {
        addListener(fn: (message: any, sender: any, respond: (r: unknown) => void) => unknown) {
          runtimeListeners.push(fn);
        },
      },
      /**
       * One request in, one answer out — the same shape the browser gives the
       * popup. The first listener that returns `true` owns the answer; if none
       * does, the caller gets `undefined`, which is exactly the case where the
       * worker never spoke.
       */
      sendMessage: (message: Record<string, unknown>) =>
        new Promise((resolve) => {
          sentMessages.push(message);
          let answered = false;
          const respond = (reply: unknown) => {
            if (answered) return;
            answered = true;
            resolve(reply);
          };
          for (const fn of runtimeListeners) {
            if (fn(message, { id: 'w87-test', tab: { id: 1 } }, respond) === true) return;
          }
          respond(undefined);
        }),
      sendNativeMessage: (name: string, message: unknown) => host.sendNativeMessage(name, message),
    },
    storage: {
      local: memoryArea(),
      onChanged: {
        addListener(fn: (changes: Record<string, unknown>, area: string) => void) {
          changeListeners.push(fn as never);
        },
      },
    },
    action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
    alarms: {
      create(name: string, info: unknown) {
        alarmBook.set(name, info);
        alarmOps.push(`create ${name}`);
      },
      async clear(name: string) {
        await throughGate('clear', name);
        const had = alarmBook.delete(name);
        alarmOps.push(`clear ${name}`);
        return had;
      },
      async get(name: string) {
        await throughGate('get', name);
        return alarmBook.get(name) ?? undefined;
      },
      onAlarm: {
        addListener(fn: (alarm: { name?: string }) => void) {
          alarmListeners.push(fn);
        },
      },
    },
    tabs: {
      async create({ url }: { url: string }) {
        return { id: 1, url };
      },
      async query() {
        return [];
      },
      async sendMessage() {
        throw new Error('Could not establish connection. Receiving end does not exist.');
      },
    },
  };
}

/** Let every already-queued microtask and macrotask turn run. */
async function settle(turns = 12): Promise<void> {
  for (let i = 0; i < turns; i += 1) await new Promise((r) => setTimeout(r, 0));
}

/** Boot the popup against the popup's own markup. The module attaches every listener. */
async function mountPopupModule(): Promise<void> {
  mountPopup();
  await import('../entrypoints/popup/main.ts');
}

/** Boot the service worker: registers its listeners and runs `backgroundSetup`. */
async function bootBackground(): Promise<void> {
  // `defineBackground` is stubbed to the identity in `beforeEach`, so `default`
  // is the callback itself — the same seam tests/c19-runit.test.ts uses.
  const mod = (await import('../entrypoints/background')) as unknown as {
    default: () => void | Promise<void>;
  };
  await mod.default();
}

/** Flip the popup's real checkbox, the way a click does. */
function toggle(on: boolean): void {
  const box = document.getElementById('toggle') as HTMLInputElement;
  box.checked = on;
  box.dispatchEvent(new Event('change'));
}

beforeEach(() => {
  for (const key of Object.keys(store)) delete store[key];
  changeListeners.length = 0;
  alarmBook.clear();
  alarmOps.length = 0;
  alarmListeners.length = 0;
  runtimeListeners.length = 0;
  sentMessages.length = 0;
  gateOp = null;
  gatePromise = null;
  releaseGate = null;
  host = createSyntheticHost({ up: true });
  vi.stubGlobal('defineBackground', (cb: unknown) => cb);
  const browser = withI18n(fakeBrowser());
  vi.stubGlobal('browser', browser);
  vi.stubGlobal('chrome', browser);
  vi.resetModules();
});

describe('W87 · the popup asks the service worker; it does not touch the alarms', () => {
  it('🔴 the popup\'s toggle performs no alarm operation of its own — it writes the switch and asks', async () => {
    // 🔴 No background module in this process, so every call recorded below is
    //    the popup's. That is what makes "the popup must not touch alarms" an
    //    assertion rather than an inference.
    await mountPopupModule();
    await settle();

    toggle(true);
    await settle();

    // The switch really did change, so the empty alarm log is not the log of a
    // toggle that never happened.
    expect(store.cs_backfill_enabled_v1).toBe(true);
    expect(alarmOps).toEqual([]);
    expect(sentMessages.map((m) => m.type)).toContain('cs-backfill-sync-alarm');

    // …and the same in the off direction: the popup is symmetric about this, it
    // does not merely arm-on-write.
    toggle(false);
    await settle();
    expect(store.cs_backfill_enabled_v1).toBe(false);
    expect(alarmOps).toEqual([]);
  });

  it('🔴 off during an in-flight popup "on" sync: the switch ends off and nothing is armed', async () => {
    await mountPopupModule();
    await settle();

    // The popup's own on-sync parks inside `alarms.get`. The worker does not
    // exist yet, so nothing else can be holding this call — it is unambiguously
    // the popup's, which is exactly the writer that must not exist.
    armGate('get');
    toggle(true);
    await settle();
    expect(store.cs_backfill_enabled_v1).toBe(true);

    // The worker comes up (its startup sync arms, as it should with the switch
    // on), and the user then turns the box back off.
    await bootBackground();
    await settle();
    toggle(false);
    await settle();

    // Everything except the parked call has now settled. Letting it through is
    // the whole point: on the unfixed code it finishes the *on* path — it read
    // `true` before the box was turned off — and re-creates both alarms after
    // the worker's off-sync already cleared them.
    letGateThrough();
    await settle();

    expect(store.cs_backfill_enabled_v1).toBe(false);
    console.log('[W87] off during an in-flight popup on-sync ->', alarmOps, 'alarms now:', [...alarmBook.keys()]);
    expect(alarmBook.has('cs-backfill-tick')).toBe(false);
    expect(alarmBook.has('cs-backfill-safety')).toBe(false);
  });

  it('🔴 on during an in-flight popup "off" sync: the switch ends on and both alarms are armed', async () => {
    // The user starts from on, with both alarms armed.
    store.cs_backfill_enabled_v1 = true;
    alarmBook.set('cs-backfill-tick', { delayInMinutes: 7 });
    alarmBook.set('cs-backfill-safety', { periodInMinutes: 60 });

    await mountPopupModule();
    await settle();

    // The popup's own off-sync parks inside `alarms.clear`, again with no worker
    // in the process to compete with it.
    armGate('clear');
    toggle(false);
    await settle();
    expect(store.cs_backfill_enabled_v1).toBe(false);

    await bootBackground();
    await settle();

    // The user turns it back on. The worker arms; the parked clear has not run.
    toggle(true);
    await settle();
    expect(store.cs_backfill_enabled_v1).toBe(true);

    // Letting the earlier clear through is the failure: on the unfixed code it
    // deletes the alarms the worker just armed, and the switch says on.
    letGateThrough();
    await settle();

    console.log('[W87] on during an in-flight popup off-sync ->', alarmOps, 'alarms now:', [...alarmBook.keys()]);
    expect(store.cs_backfill_enabled_v1).toBe(true);
    expect(alarmBook.has('cs-backfill-tick')).toBe(true);
    expect(alarmBook.has('cs-backfill-safety')).toBe(true);
  });

  it('a popup toggle still takes effect promptly, with no other event', async () => {
    await bootBackground();
    await settle();
    await mountPopupModule();
    await settle();

    toggle(true);
    await settle();
    expect(store.cs_backfill_enabled_v1).toBe(true);
    console.log('[W87] toggle on ->', alarmOps, 'alarms now:', [...alarmBook.keys()]);
    expect(alarmBook.has('cs-backfill-tick')).toBe(true);
    expect(alarmBook.has('cs-backfill-safety')).toBe(true);

    toggle(false);
    await settle();
    expect(store.cs_backfill_enabled_v1).toBe(false);
    console.log('[W87] toggle off ->', alarmOps, 'alarms now:', [...alarmBook.keys()]);
    expect(alarmBook.has('cs-backfill-tick')).toBe(false);
    expect(alarmBook.has('cs-backfill-safety')).toBe(false);
  });

  it('a worker that does not answer leaves the outcome unknown — never a value', async () => {
    // No background module: the request goes out and nobody replies.
    await mountPopupModule();
    await settle();

    const warnings: string[] = [];
    const realWarn = console.warn;
    console.warn = (...args: unknown[]) => { warnings.push(args.map(String).join(' ')); };
    const logs: string[] = [];
    const realLog = console.log;
    console.log = (...args: unknown[]) => { logs.push(args.map(String).join(' ')); };
    try {
      toggle(true);
      await settle();
    } finally {
      console.warn = realWarn;
      console.log = realLog;
    }

    // The switch is still written — the outcome of the *sync* is what is unknown,
    // not the user's choice — and nothing claims an alarm state.
    expect(store.cs_backfill_enabled_v1).toBe(true);
    const alarmLines = logs.filter((l) => l.includes('backfill alarm ->'));
    expect(alarmLines).toHaveLength(1);
    expect(alarmLines[0]).toContain('unknown');
    expect(alarmLines[0]).not.toContain('cleared');
    expect(alarmLines[0]).not.toContain('created');
  });
});
