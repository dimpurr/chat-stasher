/**
 * W12 · The badge must never outlive the outbox it describes.
 *
 * Found on a real machine (2026-09-14): the toolbar tooltip went on reading
 * "chat-stasher: 1 capture(s) waiting for the chat-stasher host" while DevTools
 * showed the `chat-stasher-outbox` database holding **zero** rows and the popup
 * had already hidden its outbox section (it hides when there is nothing to
 * send). Restarting the service worker did not clear it.
 *
 * That combination is the signature of one specific half-fix: an empty outbox is
 * painted by clearing the **text** only (`lib/badge.ts`, `paint`), while the
 * tooltip is a separate slot that the browser keeps until something writes it.
 * So the number disappears from the icon and the sentence stays in the tooltip —
 * and every later worker wake recomputes the same empty plan and repaints the
 * same half, which is exactly why a restart did not help.
 *
 * 🔴 So this suite does not assert "setTitle was called". It keeps a small model
 *    of what the toolbar actually shows — text, colour and title each persist
 *    until something writes them, the way the browser's own state does — and
 *    asserts on that, because that is what a person reads on the machine.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

/** What the toolbar shows right now. Each slot persists until it is written. */
interface ToolbarState {
  text: string;
  color: string;
  /** Empty string = no custom tooltip, i.e. the browser's own default. */
  title: string;
}

/**
 * A fake `action` that remembers, instead of a fake that records calls.
 * 🔴 The distinction is the whole point of this file: a call-recording fake says
 *    "setTitle was never called", which sounds harmless; a state-modelling fake
 *    says "the toolbar still reads 1 capture(s) waiting", which is the defect.
 */
function toolbar(): { shown: ToolbarState; action: Record<string, unknown> } {
  const shown: ToolbarState = { text: '', color: '', title: '' };
  return {
    shown,
    action: {
      async setBadgeText(o: { text: string }) { shown.text = o.text; },
      async setBadgeBackgroundColor(o: { color: string }) { shown.color = o.color; },
      async setTitle(o: { title: string }) { shown.title = o.title; },
    },
  };
}

let host: SyntheticHost;

function stubBrowser(action: Record<string, unknown>): void {
  const api = withI18n({
    runtime: {
      id: 'mock-extension-id',
      onStartup: { addListener() { /* nothing to do here */ } },
      onMessage: { addListener() { /* nothing to do here */ } },
      sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
    },
    action,
  });
  vi.stubGlobal('browser', api);
  vi.stubGlobal('chrome', api);
}

beforeEach(() => {
  vi.resetModules();
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('defineBackground', (cb: unknown) => cb);
  vi.stubGlobal('defineContentScript', (cfg: unknown) => cfg);
});

async function enqueueOne(payload = '{"a":1}', name = 'chatgpt-a.json') {
  const { enqueue } = await import('../lib/outbox');
  return await enqueue(name, payload);
}

const ACKED = {
  delivered: true as const,
  status: 'stored' as const,
  shard: '0001-chatgpt-a.json',
  requestId: 'r',
  sha256: 'x',
};

describe('W12 · the toolbar must not keep a number the outbox no longer holds', () => {
  it('🔴 a matching ack empties the outbox ==> the tooltip is emptied too, not only the number', async () => {
    const { shown, action } = toolbar();
    stubBrowser(action);

    await enqueueOne();
    const { refreshBadge } = await import('../lib/badge');
    await refreshBadge();
    // The state the machine was found in, established the normal way.
    expect(shown.text).toBe('1');
    expect(shown.title).toContain('1 capture(s) waiting');

    const { drainOutbox } = await import('../lib/outbox');
    await drainOutbox({ deliver: async () => ACKED });
    await refreshBadge();

    expect(shown.text).toBe('');
    // 🔴 The half that was missing: with only the text cleared, the toolbar went
    //    on saying "1 capture(s) waiting" over an outbox holding nothing at all.
    expect(shown.title).not.toContain('capture(s) waiting');
  });

  it('🔴 a worker waking on an empty outbox clears a tooltip left behind by the previous worker', async () => {
    const { shown, action } = toolbar();
    stubBrowser(action);

    // Exactly the state a reclaimed MV3 worker leaves behind: both slots live in
    // the browser process, not in the worker, so the next worker inherits them.
    // This is the "restarting the service worker did not help" observation.
    shown.text = '1';
    shown.title = 'chat-stasher: 1 capture(s) waiting for the chat-stasher host';

    const { refreshBadge } = await import('../lib/badge');
    await refreshBadge();

    expect(shown.text).toBe('');
    expect(shown.title).not.toContain('capture(s) waiting');
  });

  it('🔴 a rejected entry keeps the alert tooltip on the toolbar (the alert must not be cleared by the same path)', async () => {
    const { shown, action } = toolbar();
    stubBrowser(action);

    await enqueueOne();
    const { drainOutbox } = await import('../lib/outbox');
    await drainOutbox({
      deliver: async () => ({
        delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
        detail: 'not a bundle', requestId: 'r', sha256: 'x',
      }),
    });
    const { refreshBadge, BADGE_ALERT_TEXT } = await import('../lib/badge');
    await refreshBadge();

    // The counterpart of the two cases above: clearing must be driven by the
    // outbox's real content, so a non-empty outbox is never cleared by accident.
    expect(shown.text).toBe(BADGE_ALERT_TEXT);
    expect(shown.title).toContain('1 rejected');
  });

  it('🔴 an outbox that cannot be read is never painted as empty or as 0 (invariant 1, at the badge)', async () => {
    const { shown, action } = toolbar();
    stubBrowser(action);

    // No IndexedDB API at all: the outbox cannot exist, so this is the strongest
    // form of "could not be read" the badge can be handed.
    delete (globalThis as any).indexedDB;

    const { refreshBadge, BADGE_ALERT_TEXT } = await import('../lib/badge');
    const plan = await refreshBadge();

    expect(plan).not.toBeNull();
    expect(plan!.text).toBe(BADGE_ALERT_TEXT);
    expect(shown.text).toBe(BADGE_ALERT_TEXT);
    expect(shown.text).not.toBe('0');
    // The tooltip has to carry the distinction the mark alone cannot: it is the
    // one place that can say "I cannot tell", and it must actually be written.
    expect(shown.title).toContain('cannot be read');
  });
});

describe('W12 · the backfill leg does not go through the outbox, so it cannot leave a stale badge', () => {
  it('a successful deliverBackfillItem changes nothing the badge reports', async () => {
    const { shown, action } = toolbar();
    stubBrowser(action);

    // A sentinel that is genuinely waiting, so "unchanged" is a real assertion
    // and not "the outbox was empty before and after".
    await enqueueOne('{"sentinel":true}', 'chatgpt-sentinel.json');
    const { summary } = await import('../lib/outbox');
    const before = await summary();
    expect(before).not.toBeNull();
    expect(before!.pending).toBe(1);

    const { deliverBackfillItem } = await import('../entrypoints/background');
    const result = await deliverBackfillItem({
      url: 'https://chat.deepseek.com/api/v0/chat/session/aaaa1111-bbbb-4000-8000-00000000ffff',
      method: 'GET',
      status: 200,
      text: JSON.stringify({
        session_id: 'aaaa1111-bbbb-4000-8000-00000000ffff',
        message: { content: 'x' },
      }),
      capturedAt: Date.now(),
    });

    // The host really did take it — so the leg ran, it was not skipped.
    expect(result).toMatchObject({ saved: true });
    expect(host.names()).toEqual(['deepseek-aaaa1111-bbbb-4000-8000-00000000ffff.json']);
    // 🔴 §10: the backfill leg's exit is the debt ledger, not the outbox. It never
    //    enqueues and never deletes, so there is no outbox change for the badge to
    //    miss — which is why the badge is not refreshed on this path.
    expect(await summary()).toEqual(before);
  });
});
