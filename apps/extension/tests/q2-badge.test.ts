/**
 * Q2 badge (W2 version) · the badge must say the outbox's real state.
 *
 * The semantics (task 8):
 *   · waiting count > 0 ⇒ show the number;
 *   · something rejected **or** the outbox is full ⇒ the alert state (not a number);
 *   · neither ⇒ an empty badge.
 *
 * 🔴 And a fourth: **when the outbox cannot be read it must not be shown as "empty" or "0"**.
 *    "I do not know" and "nothing is waiting" are two states, and the badge is the one place
 *    the user sees at a glance — nothing that turns an unknown into a known may appear on it.
 *
 * The old version ("count of captures in the last 5 minutes, cleared when stale") was deleted
 * along with the automatic-download channel — that staleness rule existed because "the count
 * lived in storage and could outlive the worker that wrote it", whereas the badge is now derived
 * from the outbox, so a disagreement is structurally impossible.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';

/**
 * 🔴 Every place imports its module dynamically: `vi.resetModules()` invalidates the module
 *    registry, and the IndexedDB connection cached in `lib/badge` is **module-level**. A static
 *    import gets the module instance from "the moment of the previous case", which keeps using
 *    the previous round's connection — so what is written and what is read come from different
 *    databases (the test goes red, and this shape does not exist in production: one SW has one
 *    module graph and one database). So each case re-imports, paired with `vi.resetModules()`.
 */
async function badgeModule() {
  return await import('../lib/badge');
}

const badgeCalls: Array<{ kind: 'text' | 'bg' | 'title'; text?: string; color?: string; title?: string }> = [];

const fakeBrowser: any = {
  action: {
    async setBadgeText(o: { text: string }) { badgeCalls.push({ kind: 'text', text: o.text }); },
    async setBadgeBackgroundColor(o: { color: string }) { badgeCalls.push({ kind: 'bg', color: o.color }); },
    async setTitle(o: { title: string }) { badgeCalls.push({ kind: 'title', title: o.title }); },
  },
  runtime: { id: 'badge-test' },
};

function last(kind: 'text' | 'bg' | 'title') {
  return [...badgeCalls].reverse().find((c) => c.kind === kind);
}

beforeEach(() => {
  badgeCalls.length = 0;
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
});

async function enqueueOne(payload = '{"a":1}', name = 'chatgpt-a.json') {
  const { enqueue } = await import('../lib/outbox');
  return await enqueue(name, payload);
}

describe('Q2 · badgeFor (a pure function)', () => {
  it('an empty outbox ⇒ no badge (not 0, and not grey)', async () => {
    const { badgeFor } = await badgeModule();
    expect(badgeFor({ pending: 0, rejected: 0, bytes: 0, capacityBytes: 100, full: false })).toBeNull();
  });

  it('N waiting ⇒ the number N plus a waiting-coloured title', async () => {
    const { badgeFor, BADGE_COLOR_WAITING } = await badgeModule();
    const plan = badgeFor({ pending: 3, rejected: 0, bytes: 10, capacityBytes: 100, full: false })!;
    expect(plan.text).toBe('3');
    expect(plan.color).toBe(BADGE_COLOR_WAITING);
    expect(plan.title).toContain('3 capture(s) waiting');
    console.log('[Q2] the badge for 3 waiting:', plan);
  });

  it('🔴 something rejected ⇒ the alert state outranks the number (a number reads as "making progress")', async () => {
    const { badgeFor, BADGE_ALERT_TEXT, BADGE_COLOR_ALERT } = await badgeModule();
    const plan = badgeFor({ pending: 5, rejected: 2, bytes: 10, capacityBytes: 100, full: false })!;
    expect(plan.text).toBe(BADGE_ALERT_TEXT);
    expect(plan.text).not.toBe('5');
    expect(plan.color).toBe(BADGE_COLOR_ALERT);
    expect(plan.title).toContain('2 rejected');
    expect(plan.title).toContain('5 capture(s) waiting');
  });

  it('🔴 the outbox is full ⇒ the alert state (even with 0 waiting: new captures are being refused)', async () => {
    const { badgeFor, BADGE_ALERT_TEXT } = await badgeModule();
    const plan = badgeFor({ pending: 0, rejected: 0, bytes: 100, capacityBytes: 100, full: true })!;
    expect(plan.text).toBe(BADGE_ALERT_TEXT);
    expect(plan.title).toContain('full');
  });

  it('🔴 the outbox cannot be read ⇒ the alert state, with a title that says outright "cannot say", never treated as empty', async () => {
    const { badgeFor, BADGE_ALERT_TEXT } = await badgeModule();
    const plan = badgeFor(null)!;
    expect(plan).not.toBeNull();
    expect(plan.text).toBe(BADGE_ALERT_TEXT);
    expect(plan.title).toContain('cannot be read');
    expect(plan.title).toContain('cannot say what is queued');
  });
});

describe('Q2 · refreshBadge (against the real outbox)', () => {
  it('an empty outbox ⇒ the badge is cleared', async () => {
    const { refreshBadge } = await badgeModule();
    await refreshBadge();
    expect(last('text')!.text).toBe('');
  });

  it('two enqueued ⇒ the badge shows 2; after both are acked ⇒ cleared', async () => {
    const { refreshBadge, BADGE_COLOR_WAITING } = await badgeModule();
    await enqueueOne('{"a":1}', 'chatgpt-a.json');
    await enqueueOne('{"a":2}', 'chatgpt-b.json');
    const plan = await refreshBadge();
    expect(plan!.text).toBe('2');
    expect(last('text')!.text).toBe('2');
    expect(last('bg')!.color).toBe(BADGE_COLOR_WAITING);

    const { drainOutbox } = await import('../lib/outbox');
    await drainOutbox({
      deliver: async () => ({
        delivered: true, status: 'stored', shard: 's', requestId: 'r', sha256: 'x',
      }),
    });
    await refreshBadge();
    expect(last('text')!.text).toBe('');
  });

  it('🔴 one rejected ⇒ the badge goes to the alert state, and the title says how many were rejected', async () => {
    const { refreshBadge, BADGE_ALERT_TEXT } = await badgeModule();
    await enqueueOne('{"a":1}', 'chatgpt-a.json');
    const { drainOutbox } = await import('../lib/outbox');
    await drainOutbox({
      deliver: async () => ({
        delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
        detail: 'not a bundle', requestId: 'r', sha256: 'x',
      }),
    });
    await refreshBadge();
    expect(last('text')!.text).toBe(BADGE_ALERT_TEXT);
    expect(last('title')!.title).toContain('1 rejected');
  });

  it('🔴 in a browser build with no action API it no-ops silently (the badge must never drag captures down with it)', async () => {
    const { refreshBadge } = await badgeModule();
    vi.stubGlobal('browser', withI18n({ runtime: { id: 'no-action' } }));
    await enqueueOne();
    // No throw and no badge API called at all — the badge is decorative and must not bubble up its own problems.
    await expect(refreshBadge()).resolves.toBeDefined();
    expect(badgeCalls).toEqual([]);
  });
});
