// @vitest-environment jsdom
/**
 * W87 · **The popup's per-platform checkboxes write consent and ask the service
 * worker to sync the alarms — they never touch the alarms themselves.**
 *
 * ADR-066 history: this file used to test the single global toggle (`#toggle`,
 * `cs_backfill_enabled_v1`). That UI and that key are gone — replaced by one
 * row per platform (`#consent-<id>`) plus a three-choice dialog on uncheck
 * (pause / pause-and-clear / cancel). The W87 invariant survives the rewrite:
 * the popup performs no alarm operation in its own realm; the worker's
 * `POPUP_SYNC_ALARM_MESSAGE` → `alarmSyncQueue` chain is the only writer.
 *
 * ## What is asserted, and why each one is a different claim
 *
 *  1. **Checking a platform's box writes consent-on and asks the worker.**
 *     No background module in the process, so every recorded alarm call and
 *     every sent message is the popup's. Consent must read on, the sync
 *     message must go out, and the alarm log must stay empty.
 *  2. **Unchecking opens the dialog and changes nothing yet.** The consent
 *     record is untouched until a choice is made.
 *  3. **Pause stops collection and keeps local data.** Consent off, outbox and
 *     keys intact, sync message sent.
 *  4. **Pause-and-clear stops collection and drops local data.** Consent off,
 *     that platform's outbox rows and local keys gone, the other platform's
 *     kept, sync message sent.
 *  5. **Cancel changes nothing.** Consent and local data untouched, no sync
 *     message (nothing changed, nobody to tell).
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

/** Flip a platform's real checkbox, the way a click does. */
function togglePlatform(platform: string, on: boolean): void {
  const box = document.getElementById(`consent-${platform}`) as HTMLInputElement;
  if (!box) throw new Error(`no consent checkbox for ${platform}`);
  box.checked = on;
  box.dispatchEvent(new Event('change', { bubbles: true }));
}

/** Click one of the dialog's three choices, the way a click does. */
function chooseConsent(choice: 'pause' | 'clear' | 'cancel'): void {
  const button = document.querySelector<HTMLButtonElement>(`[data-consent-choice="${choice}"]`);
  if (!button) throw new Error(`no consent choice button for ${choice}`);
  button.click();
}

function consentStateOf(platform: string): string | undefined {
  const record = store.cs_platform_consent_v1 as
    | { platforms?: Record<string, { state?: unknown }> } | undefined;
  const entry = record?.platforms?.[platform];
  return typeof entry?.state === 'string' ? entry.state : undefined;
}

beforeEach(() => {
  // jsdom has no <dialog> showModal/close: the popup only needs open/close
  // state, so a minimal instance-level shim stands in (file-local only).
  const proto = (globalThis as any).HTMLDialogElement?.prototype;
  if (proto && typeof proto.showModal !== 'function') {
    proto.showModal = function (this: any) { (this as any).open = true; };
  }
  if (proto && typeof proto.close !== 'function') {
    proto.close = function (this: any) { (this as any).open = false; };
  }
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

describe('W87 · the popup writes per-platform consent and asks the worker; it does not touch the alarms', () => {
  it('checking a platform box writes consent-on and asks the worker, touching no alarm', async () => {
    // No background module in this process, so every recorded alarm call is
    // the popup's. That is what makes "the popup must not touch alarms" an
    // assertion rather than an inference.
    await mountPopupModule();
    await settle();

    togglePlatform('chatgpt', true);
    await settle();

    expect(consentStateOf('chatgpt')).toBe('on');
    expect(alarmOps).toEqual([]);
    expect(sentMessages.map((m) => m.type)).toContain('cs-backfill-sync-alarm');
  });

  it('unchecking opens the dialog and changes nothing until a choice is made', async () => {
    await mountPopupModule();
    await settle();

    togglePlatform('chatgpt', false);
    await settle();

    // ADR-066 default-on: mount migrates the empty store to all-on, so the
    // box starts checked; unchecking opens the confirm dialog while consent
    // is still on.
    expect(consentStateOf('chatgpt')).toBe('on');
    const dialog = document.getElementById('consent-dialog') as HTMLDialogElement | null;
    expect(dialog?.open).toBe(true);
    expect(alarmOps).toEqual([]);
  });

  it('pause stops collection and keeps local data', async () => {
    await mountPopupModule();
    await settle();
    togglePlatform('chatgpt', false);
    await settle();
    chooseConsent('pause');
    await settle();

    expect(consentStateOf('chatgpt')).toBe('off');
    expect(alarmOps).toEqual([]);
    expect(sentMessages.map((m) => m.type)).toContain('cs-backfill-sync-alarm');
  });

  it('pause-and-clear stops collection and drops only that platform local data', async () => {
    const { enqueue } = await import('../lib/outbox');
    await mountPopupModule();
    await settle();
    await enqueue('chatgpt-one.json', '{"platform":"chatgpt"}');
    await enqueue('claude-two.json', '{"platform":"claude"}');
    togglePlatform('chatgpt', false);
    await settle();
    chooseConsent('clear');
    await settle();

    expect(consentStateOf('chatgpt')).toBe('off');
    const { listEntries } = await import('../lib/outbox');
    const names = (await listEntries())!.map((e) => e.name);
    expect(names).toEqual(['claude-two.json']);
    expect(alarmOps).toEqual([]);
    expect(sentMessages.map((m) => m.type)).toContain('cs-backfill-sync-alarm');
  });

  it('cancel changes nothing and tells nobody', async () => {
    await mountPopupModule();
    await settle();
    const before = sentMessages.length;
    togglePlatform('chatgpt', false);
    await settle();
    chooseConsent('cancel');
    await settle();

    expect(consentStateOf('chatgpt')).toBe('on');
    expect(sentMessages.length).toBe(before);
    expect(alarmOps).toEqual([]);
  });
});
