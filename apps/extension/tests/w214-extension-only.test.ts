/**
 * W214 · EXT-12 — "only the extension, no CLI" as a first-class state.
 *
 * Topology principle 8 (docs: 36-EXTENSION-TOPOLOGY.md) says a user may install
 * the extension and no CLI at all, possibly forever. That state is not an error,
 * and it is not a degraded version of the happy path — it is the *only* state a
 * new user is ever in, and it has to be honest about what it is:
 *
 *   · the captures exist only in this browser and are not a backup yet;
 *   · the first step may not assume a CLI that is not there, so it is the
 *     one-line installer and never a `chat-stasher …` command;
 *   · how much of the 256 MiB spool is in use is visible, and 80% pauses the
 *     non-urgent producer while 100% refuses new captures *without dropping
 *     anything already queued*;
 *   · when the helper does turn up, the backlog goes out at once and the popup
 *     says how much went;
 *   · "never connected" (onboarding) and "was connected, now broken" (an alarm)
 *     are two different sentences.
 *
 * 🔴 The predicate the whole file turns on is `cliKnown`. The helper's *absence*
 *    is unprovable — `hello` fails with `no-runtime-api` / `timeout` /
 *    `send-failed` / `malformed-response` / `nack`, none of which distinguishes
 *    "no helper installed" from "a helper did not answer" — so the only licence
 *    to print a `chat-stasher …` command is positive evidence that one exists.
 *    Most of the cases below exist to hold that line in both directions: never a
 *    command without evidence, and never the installer when the CLI is known.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';

import {
  NO_FAILURES,
  cliKnown,
  neverConnected,
  extensionOnlyActive,
  onlyInBrowserView,
  firstRunView,
  outboxBarView,
  deliveredView,
  renderPopup,
  popupText,
  summarizeOutbox,
  type PopupModel,
} from '../lib/popup-view';
import {
  OUTBOX_CAPACITY_BYTES,
  OUTBOX_NEAR_FULL_FRACTION,
  CONNECT_DELIVERY_KEY,
  loadConnectDelivery,
  recordConnectDelivery,
} from '../lib/outbox';
import { INSTALLER_COMMAND, INSTALLER_URL, fixCommand, installerCommand } from '../lib/ui-strings';
import { tickBlockReason } from '../lib/backfill/schedule';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const AT = Date.parse('2026-09-27T10:00:00.000Z');

/** A model with nothing set but what a case needs — every field that matters here is explicit. */
function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    now: AT,
    ...overrides,
  };
}

/** The host record after a `hello` that succeeded at least once in the past. */
const HOST_WAS_UP = { at: AT, ok: false, reason: 'timeout', lastKnownStage: '/Users/me/stage' } as const;
/** The host record when nothing on this machine has ever answered. */
const HOST_NEVER = { at: AT, ok: false, reason: 'timeout' } as const;
/** An empty-but-readable outbox with `bytes` of capacity used. */
function outboxAt(bytes: number, capacityBytes = OUTBOX_CAPACITY_BYTES) {
  return {
    pending: bytes > 0 ? 1 : 0,
    rejected: 0,
    bytes,
    capacityBytes,
    full: bytes >= capacityBytes,
    nearFull: bytes >= capacityBytes * OUTBOX_NEAR_FULL_FRACTION,
    rejectedKinds: [] as Array<{ kind: string; count: number }>,
    rejectedSamples: [] as Array<{ kind: string; detail: string }>,
  };
}

// ===========================================================================
// 1 · cliKnown / neverConnected — the evidence rule everything else rests on
// ===========================================================================
describe('W214 · what counts as evidence that a CLI exists', () => {
  it('a `hello` that just succeeded is the strongest evidence there is', () => {
    const m = model({ nativeHost: { at: AT, ok: true, stage: '/Users/me/stage' } });
    expect(cliKnown(m)).toBe(true);
    expect(neverConnected(m)).toBe(false);
  });

  it('🔴 a `hello` that succeeded in the past still proves it, even while the host is down now', () => {
    // This is the "was connected, now broken" case: lastKnownStage survives
    // failures (lib/host-status.ts), which is exactly why it can carry this.
    expect(cliKnown(model({ nativeHost: { ...HOST_WAS_UP } }))).toBe(true);
  });

  it('🔴 a failure with no stage on record is NOT evidence of absence, only absence of evidence', () => {
    const m = model({ nativeHost: { ...HOST_NEVER } });
    expect(cliKnown(m)).toBe(false);
    expect(neverConnected(m)).toBe(true);
  });

  it('🔴 no host record at all means "we have not looked" — and it may not licence a `chat-stasher …` command', () => {
    expect(cliKnown(model({ nativeHost: null }))).toBe(false);
    expect(cliKnown(model())).toBe(false);
    expect(neverConnected(model())).toBe(true);
  });

  it('the two predicates are exact negations of each other, by construction', () => {
    const hosts = [null, undefined, { ...HOST_WAS_UP }, { ...HOST_NEVER }, { at: AT, ok: true }];
    for (const nativeHost of hosts) {
      const m = model({ nativeHost: nativeHost as PopupModel['nativeHost'] });
      expect(cliKnown(m)).toBe(!neverConnected(m));
    }
  });
});

// ===========================================================================
// 2 · The fix command: never a `chat-stasher …` without evidence
// ===========================================================================
describe('W214 · the fix command follows the evidence', () => {
  it('the installer is the one-line `curl … | sh` script, and it is a JS constant, not catalog text', () => {
    expect(INSTALLER_URL).toBe('https://chatstasher.com/install.sh');
    expect(INSTALLER_COMMAND).toBe(`curl -fsSL ${INSTALLER_URL} | sh`);
    expect(installerCommand()).toBe(INSTALLER_COMMAND);
  });

  it('no evidence ⇒ the installer', () => {
    expect(fixCommand(null, false)).toBe(INSTALLER_COMMAND);
    expect(fixCommand('/Users/me/stage', false)).toBe(INSTALLER_COMMAND);
  });

  it('evidence ⇒ the `chat-stasher …` command, with the stage it reported', () => {
    expect(fixCommand('/Users/me/stage', true))
      .toBe('chat-stasher install-native-host --stage /Users/me/stage');
  });

  it('🔴 evidence but no usable stage ⇒ the placeholder, never an invented path', () => {
    expect(fixCommand(null, true)).toBe('chat-stasher install-native-host --stage <path-to-your-stage-dir>');
    expect(fixCommand('', true)).toBe('chat-stasher install-native-host --stage <path-to-your-stage-dir>');
  });
});

// ===========================================================================
// 3 · The persistent "Only in this browser — not a backup yet" notice
// ===========================================================================
describe('W214 · the persistent extension-only notice', () => {
  it('undelivered captures + a host that is not answering ⇒ the notice is present', () => {
    const view = renderPopup(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(1024) }));
    expect(view.onlyInBrowser).not.toBeNull();
    expect(view.onlyInBrowser!.title).toBe('Only in this browser — not a backup yet');
  });

  it('🔴 the two histories get two different sentences — onboarding vs alarm', () => {
    const never = onlyInBrowserView(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(1024) }));
    const broken = onlyInBrowserView(model({ nativeHost: { ...HOST_WAS_UP }, outbox: outboxAt(1024) }));
    expect(never!.reason).toContain('No helper has ever answered on this machine');
    expect(broken!.reason).toContain('has answered before and does not answer now');
    // The whole point of the split: they must not be the same sentence.
    expect(never!.reason).not.toBe(broken!.reason);
  });

  it('🔴 a host that is answering right now ⇒ no notice: delivery is imminent, not missing', () => {
    const view = onlyInBrowserView(model({
      nativeHost: { at: AT, ok: true, stage: '/Users/me/stage' },
      outbox: outboxAt(1024),
    }));
    expect(view).toBeNull();
  });

  it('an empty outbox ⇒ no notice, whatever the host is doing', () => {
    expect(onlyInBrowserView(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(0) }))).toBeNull();
  });

  it('🔴 an unreadable outbox is unknown, not empty — it does not raise the notice on a guess', () => {
    expect(extensionOnlyActive(model({ nativeHost: { ...HOST_NEVER }, outbox: null }))).toBe(false);
    expect(onlyInBrowserView(model({ nativeHost: { ...HOST_NEVER }, outbox: null }))).toBeNull();
  });

  it('a rejected-but-kept capture also counts as undelivered', () => {
    const box = { ...outboxAt(0), rejected: 1 };
    expect(extensionOnlyActive(model({ nativeHost: { ...HOST_NEVER }, outbox: box }))).toBe(true);
  });
});

// ===========================================================================
// 4 · The first-run card
// ===========================================================================
describe('W214 · the first-run card', () => {
  it('never connected + content waiting ⇒ the card, carrying the installer', () => {
    const card = firstRunView(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(1024) }));
    expect(card).not.toBeNull();
    expect(card!.command).toBe(INSTALLER_COMMAND);
    expect(card!.exportNow.length).toBeGreaterThan(0);
  });

  it('🔴 every sentence of the card is present and says what this is, why a helper, and what to do', () => {
    const card = firstRunView(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(1024) }))!;
    expect(card.title).toContain('not a backup yet');
    expect(card.body.length).toBeGreaterThan(0);
    expect(card.whyHelper.length).toBeGreaterThan(0);
    expect(card.whatNow.length).toBeGreaterThan(0);
    expect(card.installLabel.length).toBeGreaterThan(0);
  });

  it('🔴 "was connected, now broken" is the alarm, NOT onboarding — no first-run card', () => {
    expect(firstRunView(model({ nativeHost: { ...HOST_WAS_UP }, outbox: outboxAt(1024) }))).toBeNull();
  });

  it('never connected but nothing waiting ⇒ no card (there is nothing to rescue yet)', () => {
    expect(firstRunView(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(0) }))).toBeNull();
  });

  it('🔴 nothing in the card is a `chat-stasher …` command, and nothing in the catalog carries a pipe', () => {
    const card = firstRunView(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(1024) }))!;
    const sentences = [card.title, card.body, card.whyHelper, card.whatNow, card.installLabel];
    for (const s of sentences) {
      expect(s).not.toContain('chat-stasher install');
      // ` | ` is what @wxt-dev/i18n reserves for plural forms, so no catalog entry
      // may contain it — the installer line is a JS constant precisely because of this.
      expect(s).not.toContain(' | ');
    }
  });

  it('🔴 the card only appears in the never-connected half — the alarm half shows no installer', () => {
    const never = popupText(renderPopup(model({ nativeHost: { ...HOST_NEVER }, outbox: outboxAt(1024) })));
    const broken = popupText(renderPopup(model({ nativeHost: { ...HOST_WAS_UP }, outbox: outboxAt(1024) })));
    expect(never).toContain(INSTALLER_COMMAND);
    expect(broken).not.toContain(INSTALLER_COMMAND);
  });
});

// ===========================================================================
// 5 · The outbox usage bar (256 MiB cap; 80% pause; 100% refuse, never drop)
// ===========================================================================
describe('W214 · the outbox usage bar', () => {
  it('the cap and the near-full line are named constants, and 80% is the line', () => {
    expect(OUTBOX_CAPACITY_BYTES).toBe(256 * 1024 * 1024);
    expect(OUTBOX_NEAR_FULL_FRACTION).toBe(0.8);
  });

  it('summarize() marks nearFull at the threshold, and full keeps its own meaning', async () => {
    // 🔴 The cap is a power of two and 0.8 is not, so `cap * 0.8` lands between
    //    two integers. `Math.ceil` is therefore the first byte count that trips
    //    the line, and the assertion is written against that rather than against
    //    a rounded-off 80% that the comparison does not actually use.
    const at = Math.ceil(OUTBOX_CAPACITY_BYTES * OUTBOX_NEAR_FULL_FRACTION);
    const below = summarizeOutbox([
      { sha256: 'a'.repeat(64), name: 'n', payload: 'p', bytes: at - 1, enqueuedAt: 1, attempts: 0, lastError: null, lastAttemptAt: null, state: 'pending' },
    ]);
    const on = summarizeOutbox([
      { sha256: 'b'.repeat(64), name: 'n', payload: 'p', bytes: at, enqueuedAt: 1, attempts: 0, lastError: null, lastAttemptAt: null, state: 'pending' },
    ]);
    expect(below.nearFull).toBe(false);
    expect(on.nearFull).toBe(true);
    // 80% is a warning, never a refusal: the refusal is `full`, and it is not raised here.
    expect(on.full).toBe(false);
  });

  it('nothing staged and not full ⇒ no bar at all (an empty bar is noise)', () => {
    expect(outboxBarView(model({ outbox: outboxAt(0, 1000) }))).toBeNull();
  });

  it('🔴 an unreadable outbox ⇒ no bar, never a 0-width one (unknown is not empty)', () => {
    expect(outboxBarView(model({ outbox: null }))).toBeNull();
  });

  it('below the line ⇒ a plain bar with no state sentence', () => {
    const bar = outboxBarView(model({ outbox: outboxAt(100, 1000) }))!;
    expect(bar.pct).toBe(10);
    expect(bar.nearFull).toBe(false);
    expect(bar.full).toBe(false);
    expect(bar.stateLine).toBeNull();
    expect(bar.caption).toContain('100 B');
    expect(bar.caption).toContain('1000 B');
  });

  it('🔴 at or above 80% ⇒ the near-full state line, and it says backfill paused (not "full")', () => {
    const bar = outboxBarView(model({ outbox: outboxAt(800, 1000) }))!;
    expect(bar.pct).toBe(80);
    expect(bar.nearFull).toBe(true);
    expect(bar.full).toBe(false);
    expect(bar.stateLine).toContain('80%');
    expect(bar.stateLine).toContain('backfill');
    // Near-full is not full: it must not borrow the refusal sentence.
    expect(bar.stateLine).not.toContain('refused');
  });

  it('🔴 full ⇒ the red state, and it says new captures are refused while nothing queued was deleted', () => {
    const bar = outboxBarView(model({ outbox: outboxAt(1000, 1000) }))!;
    expect(bar.pct).toBe(100);
    expect(bar.full).toBe(true);
    expect(bar.stateLine).toContain('FULL');
    expect(bar.stateLine).toContain('refused');
    expect(bar.stateLine).toContain('Nothing queued was deleted');
    // `nearFull` is reported separately from `full` so the style layer can tell
    // amber from red without re-deriving the comparison.
    expect(bar.nearFull).toBe(false);
  });

  it('the percentage is clamped into 0..100 even if the recorded bytes exceed the cap', () => {
    const bar = outboxBarView(model({ outbox: outboxAt(5000, 1000) }))!;
    expect(bar.pct).toBe(100);
    expect(bar.full).toBe(true);
  });
});

// ===========================================================================
// 6 · The "delivered N" record written when the host first connects
// ===========================================================================
describe('W214 · the delivered-on-connect record', () => {
  it('nothing recorded ⇒ no line', async () => {
    expect(deliveredView(model())).toBeNull();
    expect(deliveredView(model({ delivered: null }))).toBeNull();
  });

  it('a zero count is not a delivery ⇒ no line (never "delivered 0")', () => {
    expect(deliveredView(model({ delivered: { at: AT, count: 0 } }))).toBeNull();
  });

  it('N delivered ⇒ a line naming the count', () => {
    const line = deliveredView(model({ delivered: { at: AT, count: 7 } }))!;
    expect(line).toContain('7');
    expect(line).toContain('archive');
  });

  it('🔴 the line carries the moment it happened — a record from months ago cannot read as "just now"', () => {
    // 🔴 The record is written once and never cleared, so the time is the only
    //    thing that keeps a stale drain from reading as a fresh one. `at` is why
    //    the record stores a time at all (W214b review, finding 2).
    const old = deliveredView(model({ delivered: { at: AT, count: 7 } }))!;
    const other = deliveredView(model({ delivered: { at: AT - 90 * 24 * 60 * 60 * 1000, count: 7 } }))!;
    expect(old).toContain('2026-09-27');
    // Same count, different moment ⇒ the two lines are not identical, which is the
    // whole property: without `at` they would be the same string.
    expect(other).not.toBe(old);
  });

  it('the record round-trips through the store, and a malformed one reads as absent rather than as zero', async () => {
    const mem = new Map<string, unknown>();
    const store = {
      load: async (k: string) => mem.get(k) ?? null,
      save: async (k: string, v: unknown) => void mem.set(k, v),
      remove: async (k: string) => void mem.delete(k),
      keys: async () => [...mem.keys()],
    } as never;
    expect(await loadConnectDelivery(store)).toBeNull();
    await recordConnectDelivery(store, { at: AT, count: 3 });
    expect(await loadConnectDelivery(store)).toEqual({ at: AT, count: 3 });
    // A record missing its count is not a record of zero deliveries.
    mem.set(CONNECT_DELIVERY_KEY, { at: AT });
    expect(await loadConnectDelivery(store)).toBeNull();
    // And an absent store is unknown, not empty.
    expect(await loadConnectDelivery(null)).toBeNull();
  });
});

// ===========================================================================
// 7 · The schedule gate: 80% pauses the non-urgent producer
// ===========================================================================
describe('W214 · the near-full spool pauses backfill, not capture', () => {
  const base = {
    hasStore: true,
    isEnabled: () => true,
    isHostPaused: () => false,
    hasHttp: true,
    hasTargets: true,
  };

  it('🔴 near-full ⇒ the tick is blocked, with its own reason', async () => {
    expect(await tickBlockReason({ ...base, isOutboxNearFull: () => true })).toBe('outbox-near-full');
  });

  it('not near-full ⇒ the tick is not blocked by this gate', async () => {
    expect(await tickBlockReason({ ...base, isOutboxNearFull: () => false })).not.toBe('outbox-near-full');
  });

  it('the gate is optional: a caller that never asks keeps the behaviour it had', async () => {
    expect(await tickBlockReason({ ...base })).not.toBe('outbox-near-full');
  });

  it('🔴 an unreachable host still outranks a congested spool — the more specific cause wins', async () => {
    expect(await tickBlockReason({ ...base, isHostPaused: () => true, isOutboxNearFull: () => true }))
      .toBe('host-paused');
  });

  it('🔴 the reason is worded everywhere it surfaces, and never as "the host is unreachable"', () => {
    const view = renderPopup(model({ block: 'outbox-near-full' }));
    expect(view.running).toContain('80%');
    expect(view.missing).toContain('80%');
    // The helper may be answering perfectly well here; borrowing the host-paused
    // sentence would be a wrong explanation of a correct pause.
    expect(view.running).not.toContain('host is unreachable');
    expect(view.missing).not.toContain('host is unreachable');
  });
});

// ===========================================================================
// 8 · The stage evidence survives a probe that was already in flight
// ===========================================================================
describe('W214 · a failing probe cannot erase the stage evidence', () => {
  /**
   * 🔴 Found by the e2e spec, not by reading the code: `checkHost` is a
   *    read-modify-write on one key, and two probes can interleave so that a
   *    failure built from a record with no stage is written *after* another writer
   *    stored the evidence. The popup then tells a user who has a working CLI to
   *    install it — the exact inversion of topology principle 8's rule.
   *
   *    The interleaving is reproduced deterministically here rather than raced
   *    for: the store answers `null` to the probe's own read (it ran too early)
   *    and the stored record to every later read (the evidence landed in between).
   *    That is the whole of the race, without depending on timing.
   */
  function racyStore(firstReadIsStale: boolean) {
    const stored: Record<string, unknown> = {};
    let reads = 0;
    const store = {
      async load(key: string) {
        reads += 1;
        if (firstReadIsStale && reads === 1) return null;
        return stored[key] ?? null;
      },
      async save(key: string, value: unknown) { stored[key] = value; },
      async remove(key: string) { delete stored[key]; },
      async keys() { return Object.keys(stored); },
    };
    return { store, stored };
  }

  it('🔴 a `hello` that fails after the evidence landed keeps it, on disk and in the result', async () => {
    const { checkHost, HOST_STATUS_KEY, loadHostStatus } = await import('../lib/host-status');
    const { store, stored } = racyStore(true);
    // The evidence, written by another probe while this one was in flight.
    stored[HOST_STATUS_KEY] = {
      at: AT - 1000, ok: true, stage: '/Users/me/stage', lastKnownStage: '/Users/me/stage',
    };

    // No `sendNativeMessage` in this environment, so `hello` fails deterministically.
    const record = await checkHost(store as never, { now: AT });

    expect(record.ok).toBe(false);
    expect(record.lastKnownStage).toBe('/Users/me/stage');
    // And what is on disk is what the caller was told — not a second answer.
    expect((stored[HOST_STATUS_KEY] as { lastKnownStage?: string }).lastKnownStage)
      .toBe('/Users/me/stage');
    expect((await loadHostStatus(store as never))?.lastKnownStage).toBe('/Users/me/stage');
  });

  it('a plain failure (no race) also keeps it — the merge never makes things worse', async () => {
    const { checkHost, HOST_STATUS_KEY } = await import('../lib/host-status');
    const { store, stored } = racyStore(false);
    stored[HOST_STATUS_KEY] = { at: AT - 1000, ok: false, reason: 'timeout', lastKnownStage: '/old/stage' };

    const record = await checkHost(store as never, { now: AT });

    expect(record.ok).toBe(false);
    expect(record.lastKnownStage).toBe('/old/stage');
    expect((stored[HOST_STATUS_KEY] as { lastKnownStage?: string }).lastKnownStage).toBe('/old/stage');
  });

  it('a success is still what replaces the stage — the evidence is not frozen', async () => {
    const { toRecord } = await import('../lib/host-status');
    // `toRecord` is the only thing that decides the field, and on a successful
    // `hello` it takes the stage the host just reported, whatever came before.
    const record = toRecord(
      { ok: true, machine: 'm', stage: '/new/stage', hostVersion: '1' },
      AT,
      { at: AT - 1000, ok: false, reason: 'timeout', lastKnownStage: '/old/stage' },
    );
    expect(record.lastKnownStage).toBe('/new/stage');
  });
});

// ===========================================================================
// 9 · Drain on first connect (the real outbox, a synthetic host)
// ===========================================================================
describe('W214 · the backlog drains by itself when the host first connects', () => {
  let localValues: Record<string, unknown>;
  let host: SyntheticHost;
  /** The listeners the **real** entry point registers, so a case can fire the wake-up itself. */
  let alarmListeners: Array<(a: { name?: string }) => void>;
  let messageListeners: Array<(m: unknown, s: unknown, r: (x: unknown) => void) => unknown>;

  /**
   * 🔴 The host is the suite's own `createSyntheticHost`, not a hand-rolled stub.
   *    A stub that answers `deliver` without the sha256 the ack is matched on does
   *    not produce a delivery at all — it produces a drain that stops after one
   *    item — and a test built on it would be asserting the stub's mistake rather
   *    than the extension's behaviour. Reusing the shared host also means this file
   *    cannot drift from the protocol the rest of the suite exercises.
   */
  const browserLike = () => ({
    runtime: {
      id: 'mock-extension-id',
      onStartup: { addListener() { /* the badge refresh on startup is not under test */ } },
      onMessage: { addListener(fn: (m: unknown, s: unknown, r: (x: unknown) => void) => unknown) { messageListeners.push(fn); } },
      sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
    },
    action: {
      async setBadgeText() {},
      async setBadgeBackgroundColor() {},
      async setTitle() {},
    },
    alarms: {
      create() {},
      clear: async () => true,
      onAlarm: { addListener(fn: (a: { name?: string }) => void) { alarmListeners.push(fn); } },
    },
    storage: {
      local: {
        async get(query: Record<string, unknown> | null) {
          if (query === null) return { ...localValues };
          return Object.fromEntries(
            Object.entries(query).map(([k, fallback]) => [k, k in localValues ? localValues[k] : fallback]),
          );
        },
        async set(values: Record<string, unknown>) { Object.assign(localValues, values); },
        async remove(keys: string[]) { for (const k of keys) delete localValues[k]; },
      },
    },
  });

  beforeEach(() => {
    vi.resetModules();
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
    localValues = {};
    host = createSyntheticHost({ up: true });
    alarmListeners = [];
    messageListeners = [];
    const fake = browserLike();
    vi.stubGlobal('chrome', fake);
    vi.stubGlobal('browser', fake);
    vi.stubGlobal('defineBackground', (cb: unknown) => cb);
    vi.stubGlobal('defineContentScript', (cfg: unknown) => cfg);
  });

  it('🔴 the backlog goes out at once, and the delivered count is recorded once', async () => {
    const ob = await import('../lib/outbox');
    await ob.enqueue('chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json', '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}');
    await ob.enqueue('chatgpt-bbbbbbbb-1111-2222-3333-444444444444.json', '{"sessionId":"bbbbbbbb-1111-2222-3333-444444444444"}');
    expect((await ob.summary())!.pending).toBe(2);

    const bg = await import('../entrypoints/background');
    const store = await import('../lib/backfill/store');
    const real = store.browserLocalStore();

    await bg.hostConnectedDrain(real);

    // The two queued captures reached the host, without waiting for the alarm.
    expect(host.names().length).toBe(2);
    expect((await ob.summary())!.pending).toBe(0);
    // And the popup can now say how many.
    expect(await ob.loadConnectDelivery(real)).toEqual({ at: expect.any(Number), count: 2 });
  });

  it('🔴 a second drain does not overwrite the first delivery record', async () => {
    const ob = await import('../lib/outbox');
    await ob.enqueue('chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json', '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}');
    const bg = await import('../entrypoints/background');
    const store = await import('../lib/backfill/store');
    const real = store.browserLocalStore();

    await bg.hostConnectedDrain(real);
    const first = await ob.loadConnectDelivery(real);
    expect(first!.count).toBe(1);

    await ob.enqueue('chatgpt-cccccccc-1111-2222-3333-444444444444.json', '{"sessionId":"cccccccc-1111-2222-3333-444444444444"}');
    await bg.hostConnectedDrain(real);

    // The record keeps the *first* connect's count; it is not a running total, and
    // it is not rewritten to the later drain's number either.
    expect(host.names().length).toBe(2);
    expect(await ob.loadConnectDelivery(real)).toEqual(first);
  });

  it('an empty outbox ⇒ nothing is sent and no record is written', async () => {
    const bg = await import('../entrypoints/background');
    const store = await import('../lib/backfill/store');
    const real = store.browserLocalStore();

    await bg.hostConnectedDrain(real);

    expect(host.names().length).toBe(0);
    const ob = await import('../lib/outbox');
    expect(await ob.loadConnectDelivery(real)).toBeNull();
  });

  // -------------------------------------------------------------------------
  // W214b · The drain the popup never sees (review, finding 1)
  //
  // The record used to be written from `hostConnectedDrain` alone, which only the
  // popup's probe calls. The 5-minute outbox alarm and the post-enqueue drain
  // empty the same spool, and when either of them gets there first the notice
  // simply never appeared — "tell the user how much went out" silently did not
  // happen, in exactly the flow the extension-only user is in.
  // -------------------------------------------------------------------------

  /** Two captures waiting, and a pause on record — which is what makes a drain a *connect*. */
  async function backlogWithPause(): Promise<{ ob: typeof import('../lib/outbox'); store: ReturnType<typeof import('../lib/backfill/store')['browserLocalStore']> }> {
    const ob = await import('../lib/outbox');
    await ob.enqueue('chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json', '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}');
    await ob.enqueue('chatgpt-bbbbbbbb-1111-2222-3333-444444444444.json', '{"sessionId":"bbbbbbbb-1111-2222-3333-444444444444"}');
    const store = (await import('../lib/backfill/store')).browserLocalStore();
    // The helper had been recorded away — the record that tells the notice it is
    // reporting a connect rather than an ordinary delivery.
    const { setHostPause } = await import('../lib/host-status');
    await setHostPause(store, { reason: 'host-unavailable', at: AT });
    return { ob, store };
  }

  it('🔴 the 5-minute outbox alarm records the connect delivery too — the popup never has to open', async () => {
    const { ob, store } = await backlogWithPause();
    expect(await ob.loadConnectDelivery(store)).toBeNull();

    const { OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    // `any`: the entry point's default export is the background definition, which
    // the suite stubs `defineBackground` to make callable (the same shape the other
    // wiring suites use).
    const bg: any = await import('../entrypoints/background');
    await bg.default();
    for (const fn of alarmListeners) fn({ name: OUTBOX_ALARM_NAME });
    await bg.outboxAlarmSettled();

    expect(host.names().length).toBe(2);
    expect(await ob.loadConnectDelivery(store)).toEqual({ at: expect.any(Number), count: 2 });
  });

  it('🔴 the drain that follows a capture records it as well', async () => {
    const { ob, store } = await backlogWithPause();

    const bg: any = await import('../entrypoints/background');
    await bg.default();
    const sid = 'cccccccc-1111-2222-3333-444444444444';
    const payload = {
      url: `https://chatgpt.com/backend-api/conversation/${sid}`,
      method: 'GET',
      status: 200,
      text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-1' }),
      pageUrl: `https://chatgpt.com/c/${sid}`,
      capturedAt: AT,
    };
    await new Promise<unknown>((resolve) => {
      for (const fn of messageListeners) fn({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    });

    // The two that were already waiting, plus the one this very capture added.
    expect(host.names().length).toBe(3);
    expect(await ob.loadConnectDelivery(store)).toEqual({ at: expect.any(Number), count: 3 });
  });

  it('🔴 no pause on record ⇒ an ordinary capture writes no notice (the gate keeps it from being universal noise)', async () => {
    const ob = await import('../lib/outbox');
    await ob.enqueue('chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json', '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}');
    const store = (await import('../lib/backfill/store')).browserLocalStore();

    const { OUTBOX_ALARM_NAME } = await import('../lib/outbox-alarm');
    // `any`: the entry point's default export is the background definition, which
    // the suite stubs `defineBackground` to make callable (the same shape the other
    // wiring suites use).
    const bg: any = await import('../entrypoints/background');
    await bg.default();
    for (const fn of alarmListeners) fn({ name: OUTBOX_ALARM_NAME });
    await bg.outboxAlarmSettled();

    // It really was delivered…
    expect(host.names().length).toBe(1);
    // …and it is still not a connect: the helper never went away, so the permanent
    // "delivered 1" line would have been noise on every install.
    expect(await ob.loadConnectDelivery(store)).toBeNull();
  });
});

// ===========================================================================
// 10 · 🔴 The runtime tick reads the real spool (W214b review, the blocker)
//
// The gate was a sentence on the popup and nothing else: `isOutboxNearFull` was
// supplied by entrypoints/popup/main.ts alone, so one screen said "the backfill
// leg paused until it drains" while `tickBackfill` — the only runtime entry
// point — kept fetching and delivering. The cases below drive **the tick itself**
// against a real spool and never hand it a near-full predicate: a test that
// injected one would assert the gate function, which was never the broken part.
// ===========================================================================
describe('W214b · an 80%-full spool stops the runtime tick, not just the popup', () => {
  /**
   * Put one entry into the **real** outbox with a `bytes` of its own.
   *
   * 🔴 `bytes` is the product's own accounting and the only thing `summary()`
   *    adds up, so a case that needs "a spool that is 82% full" seeds that number
   *    rather than 210 MiB of payload — the technique the browser spec uses for
   *    the same reason (e2e/harness.ts `seedOutbox`). Nothing here is delivered.
   */
  /**
   * 🔴 The tick and the spool are imported **inside** each case, on the module
   *    instance `beforeEach` has just reset. A static import would hold the copy
   *    whose outbox connection was opened against the previous case's IndexedDB
   *    factory — so the seeded spool and the tick would be looking at two
   *    different databases, and the case would pass or fail for the wrong reason.
   */
  const schedule = () => import('../lib/backfill/schedule');

  async function seedSpool(bytes: number): Promise<void> {
    const ob = await import('../lib/outbox');
    // Let the product create its own schema first, so this helper cannot drift
    // from the real stores and indexes.
    await ob.summary();
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const req = indexedDB.open(ob.OUTBOX_DB_NAME, ob.OUTBOX_DB_VERSION);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
    try {
      await new Promise<void>((resolve, reject) => {
        const tx = db.transaction(ob.OUTBOX_STORE, 'readwrite');
        tx.objectStore(ob.OUTBOX_STORE).put({
          sha256: 'a'.repeat(64),
          name: 'chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json',
          payload: '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}',
          bytes,
          enqueuedAt: AT - 1000,
          attempts: 0,
          lastError: null,
          lastAttemptAt: null,
          state: 'pending',
        });
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
        tx.onabort = () => reject(tx.error);
      });
    } finally {
      db.close();
    }
  }

  /** A store with the backfill switch already on, keyed by the module's own constant. */
  async function tickStore() {
    const key = (await schedule()).BACKFILL_ENABLED_KEY;
    const mem = new Map<string, unknown>([[key, true]]);
    return {
      load: async (k: string) => mem.get(k) ?? null,
      save: async (k: string, v: unknown) => void mem.set(k, v),
      remove: async (k: string) => void mem.delete(k),
      keys: async () => [...mem.keys()],
    };
  }

  /** One tick, on the module instance the case has been setting up. */
  async function runTick() {
    const s = await schedule();
    // The single-flight lock is module state; a previous case leaves it set if it
    // returned early, and then this tick would answer 'already-running'.
    s.resetTickLockForTest();
    return await s.tickBackfill({
      store: (await tickStore()) as never,
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'default',
    });
  }

  beforeEach(() => {
    vi.resetModules();
    (globalThis as unknown as { indexedDB: IDBFactory }).indexedDB = new IDBFactory();
  });

  it('🔴 an 82%-full spool ⇒ the real tick is blocked, and it never asks to fetch', async () => {
    await seedSpool(Math.ceil(OUTBOX_CAPACITY_BYTES * 0.82));
    // 🔴 No `http` port and no injected predicate. Without the wiring this tick
    //    walks straight past the gate and answers `no-http-port` — which is
    //    exactly the failing result this case exists to catch.
    const result = await runTick();
    expect(result.ran).toBe(false);
    expect(result.reason).toBe('outbox-near-full');
  });

  it('a spool below the line ⇒ the gate is not stuck on (the tick answers for the port instead)', async () => {
    await seedSpool(Math.floor(OUTBOX_CAPACITY_BYTES * 0.79));
    const result = await runTick();
    expect(result.reason).not.toBe('outbox-near-full');
  });

  it('🔴 the probe itself: empty ⇒ no, 82% ⇒ yes, unreadable ⇒ no (an unknown is not a pause)', async () => {
    expect(await (await schedule()).productionOutboxNearFull()).toBe(false);
    await seedSpool(Math.ceil(OUTBOX_CAPACITY_BYTES * 0.82));
    expect(await (await schedule()).productionOutboxNearFull()).toBe(true);
    // 🔴 A context with no IndexedDB at all: `summary()` says `null` — "could not
    //    be read" — and that must not become a pause, or archiving would stop for
    //    a reason nobody can name (lib/outbox.ts's three-state rule).
    vi.resetModules();
    delete (globalThis as { indexedDB?: IDBFactory }).indexedDB;
    expect(await (await schedule()).productionOutboxNearFull()).toBe(false);
  });
});
