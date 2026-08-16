import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  isStale,
  recordCapture,
  refreshBadge,
  BADGE_KEY_COUNT,
  BADGE_KEY_LAST_AT,
  BADGE_STALE_MS,
  BADGE_COLOR_ACTIVE,
} from '../lib/badge';

/**
 * Q2 badge tests. Mirror of the runtime surface we depend on (storage.local +
 * action), driven through the real lib/badge.ts functions — no file I/O, no
 * conversations, nothing but counters and timestamps.
 */

type Store = Record<string, any>;
const store: Store = {};
const badgeCalls: Array<{ kind: 'text' | 'bg'; text?: string; color?: string }> = [];
const startupListeners: Array<() => void> = [];

const fakeBrowser: any = {
  storage: {
    local: {
      async get(defaults: Record<string, unknown>): Promise<Record<string, unknown>> {
        const out: Record<string, unknown> = { ...defaults };
        for (const k of Object.keys(defaults)) if (store[k] !== undefined) out[k] = store[k];
        return out;
      },
      async set(values: Store): Promise<void> {
        Object.assign(store, values);
      },
      async remove(keys: string[]): Promise<void> {
        for (const k of keys) delete store[k];
      },
    },
  },
  action: {
    async setBadgeText(o: { text: string }): Promise<void> {
      badgeCalls.push({ kind: 'text', text: o.text });
    },
    async setBadgeBackgroundColor(o: { color: string }): Promise<void> {
      badgeCalls.push({ kind: 'bg', color: o.color });
    },
  },
  runtime: {
    onStartup: {
      addListener(fn: () => void): void {
        startupListeners.push(fn);
      },
    },
  },
};

function lastBadgeText(): string {
  return [...badgeCalls].reverse().find((c) => c.kind === 'text')?.text ?? '(none)';
}

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  badgeCalls.length = 0;
  startupListeners.length = 0;
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

describe('Q2 badge visibility', () => {
  it('recordCapture bumps the count and paints the badge (1, then 2)', async () => {
    await recordCapture();
    expect(store[BADGE_KEY_COUNT]).toBe(1);
    expect(typeof store[BADGE_KEY_LAST_AT]).toBe('number');
    const calls = badgeCalls.filter((c) => c.kind === 'text');
    expect(calls.at(-1)!.text).toBe('1');
    expect(badgeCalls.find((c) => c.kind === 'bg')!.color).toBe(BADGE_COLOR_ACTIVE);

    await recordCapture();
    expect(store[BADGE_KEY_COUNT]).toBe(2);
    expect(lastBadgeText()).toBe('2');
    console.log('[Q2] after two captures badge text =', lastBadgeText());
  });

  it('SW wake with a STALE lastCaptureAt clears the badge and resets the session', async () => {
    // Pretend a long-dead worker left "5" on the badge from 10 minutes ago.
    store[BADGE_KEY_COUNT] = 5;
    store[BADGE_KEY_LAST_AT] = Date.now() - 10 * 60 * 1000;

    await refreshBadge();

    expect(lastBadgeText()).toBe('');
    expect(store[BADGE_KEY_COUNT]).toBeUndefined();
    expect(store[BADGE_KEY_LAST_AT]).toBeUndefined();
    console.log('[Q2] stale session -> badge cleared, counters reset');
  });

  it('SW wake with a FRESH lastCaptureAt restores the persisted count', async () => {
    store[BADGE_KEY_COUNT] = 3;
    store[BADGE_KEY_LAST_AT] = Date.now() - 1000;

    await refreshBadge();

    expect(lastBadgeText()).toBe('3');
    console.log('[Q2] fresh session after SW recycle -> badge restored to', lastBadgeText());
  });

  it('purity: isStale boundary', () => {
    const now = 1_700_000_000_000;
    expect(isStale(undefined, now)).toBe(true);
    expect(isStale(now - BADGE_STALE_MS, now)).toBe(false); // exactly on the edge: still fresh
    expect(isStale(now - BADGE_STALE_MS - 1, now)).toBe(true);
  });
});