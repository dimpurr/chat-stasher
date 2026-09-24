/**
 * W91 · **One typed platform list, and the release channel that decides which
 * rows it serves.**
 *
 * The owner decision of 2026-09-24 is that two platforms (Perplexity and Kimi)
 * keep being developed but must not block the stable release: both are
 * `experimental`, so a **stable** build ships them fully inert — no content-script
 * match, no backfill target, no enumeration, no tick — while a **dev** build works
 * exactly as before.
 *
 * ## What this file pins, and why each half is separate
 *
 *  1. **The list is the one source of truth.** `ALL_PLATFORMS` carries a `channel`
 *     per row, and the stable/dev split is asserted by id rather than derived from
 *     whatever the table happens to say — a new experimental row must be seen here.
 *  2. **The derived sets follow the list.** `platformsForChannel('stable')` drops
 *     the experimental rows, and `contentMatchesForChannel` drops their origins, so
 *     the manifest cannot carry an origin the table does not activate.
 *  3. **A leftover registry row is ignored, not deleted.** This is the runtime half
 *     of "fully inert": a dev build's stored target/tab for an experimental origin
 *     must not be served by a stable build, and must survive untouched in storage
 *     so switching back to a dev build finds it again (CLAUDE.md invariant 3 — the
 *     stable build changes nothing about data it does not own).
 *  4. **The engine and the tick refuse it by name** even when they are handed the
 *     origin directly, so the safety does not depend on the registry filter above.
 *
 * 🔴 The suite runs with the build channel pinned to `dev` (`vitest.config.ts`,
 *    the same key a real build defines), so the production constants here are the
 *    dev ones. Every stable assertion therefore passes `'stable'` explicitly — the
 *    `channel` parameter on the registry/engine readers is the seam that makes
 *    that possible without rebuilding the bundle.
 */

import { describe, it, expect, vi } from 'vitest';
import {
  ALL_PLATFORMS,
  CONTENT_MATCHES,
  contentMatchesForChannel,
  currentReleaseChannel,
  findPlatformForUrl,
  getPlatformByOrigin,
  isPlatformActiveInChannel,
  PLATFORMS,
  platformsForChannel,
} from '../lib/contract';
import { BACKFILL_TARGETS_KEY, loadTargets, rememberTarget } from '../lib/backfill/alarm';
import { BACKFILL_TABS_KEY, loadTabs, rememberTab } from '../lib/backfill/tab-port';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { setBackfillEnabled, tickBackfill } from '../lib/backfill/schedule';
import { memoryStore } from '../lib/backfill/store';

const PERPLEXITY_ORIGIN = 'https://www.perplexity.ai';
const KIMI_ORIGIN = 'https://www.kimi.com';
const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';

/** The exact ids the owner decision names. Asserted, not derived: a new row must be read here. */
const STABLE_IDS = ['chatgpt', 'claude', 'deepseek', 'gemini', 'grok'];
const EXPERIMENTAL_IDS = ['kimi', 'perplexity'];

/** The two origins a stable build must not match, content-script or content-script-adjacent. */
const EXPERIMENTAL_ORIGINS = ['https://www.perplexity.ai/*', 'https://www.kimi.com/*'];

function fakeHttp(): { port: (url: string) => Promise<HttpResponse>; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    port: async (url: string) => {
      calls.push(url);
      return { status: 200, text: '{}' };
    },
  };
}

describe('W91-1 · the release channel and the one platform list', () => {
  it('🔴 every platform row carries a channel, and the split is the owner decision', () => {
    expect(currentReleaseChannel()).toBe('dev');

    const ids = ALL_PLATFORMS.map((row) => row.id);
    expect(new Set(ids).size, 'ids are unique').toBe(ids.length);
    expect([...ids].sort()).toEqual([...STABLE_IDS, ...EXPERIMENTAL_IDS].sort());

    for (const row of ALL_PLATFORMS) {
      expect(['stable', 'experimental'], `row ${row.id} channel`).toContain(row.channel);
    }
    expect(ALL_PLATFORMS.filter((r) => r.channel === 'experimental').map((r) => r.id).sort())
      .toEqual([...EXPERIMENTAL_IDS].sort());
    expect(ALL_PLATFORMS.filter((r) => r.channel === 'stable').map((r) => r.id).sort())
      .toEqual([...STABLE_IDS].sort());
  });

  it('🔴 the derived stable set drops the experimental rows, and dev keeps every row', () => {
    const stable = platformsForChannel('stable').map((r) => r.id);
    const dev = platformsForChannel('dev').map((r) => r.id);
    expect(stable).not.toContain('perplexity');
    expect(stable).not.toContain('kimi');
    expect(stable).toEqual(expect.arrayContaining(STABLE_IDS));
    expect(dev).toEqual(ALL_PLATFORMS.map((r) => r.id));
  });

  it('🔴 the content-script match set follows the channel — no experimental origin in a stable build', () => {
    const stable = contentMatchesForChannel('stable');
    const dev = contentMatchesForChannel('dev');
    for (const origin of EXPERIMENTAL_ORIGINS) {
      expect(stable, `stable must not match ${origin}`).not.toContain(origin);
      expect(dev, `dev must match ${origin}`).toContain(origin);
    }
    // The production constants, too: the suite is on `dev`, so they are the dev set.
    expect(CONTENT_MATCHES).toEqual(dev);
    expect(PLATFORMS.map((r) => r.id)).toEqual(ALL_PLATFORMS.map((r) => r.id));
  });

  it('🔴 a lookup and a URL match are answers about the channel, not about the table', () => {
    expect(isPlatformActiveInChannel('perplexity', 'stable')).toBe(false);
    expect(isPlatformActiveInChannel('kimi', 'stable')).toBe(false);
    expect(isPlatformActiveInChannel('deepseek', 'stable')).toBe(true);
    expect(isPlatformActiveInChannel('perplexity', 'dev')).toBe(true);
    expect(isPlatformActiveInChannel('not-a-platform', 'stable'), 'an unknown id is not active').toBe(false);

    expect(getPlatformByOrigin(PERPLEXITY_ORIGIN, 'stable')).toBeUndefined();
    expect(getPlatformByOrigin(KIMI_ORIGIN, 'stable')).toBeUndefined();
    expect(getPlatformByOrigin(DEEPSEEK_ORIGIN, 'stable')?.id).toBe('deepseek');

    expect(findPlatformForUrl(`${PERPLEXITY_ORIGIN}/rest/thread/synthetic`, 'stable')).toBeNull();
    expect(findPlatformForUrl(`${PERPLEXITY_ORIGIN}/rest/thread/synthetic`, 'dev')?.id).toBe('perplexity');
  });
});

describe('W91-2 · a leftover registry row is ignored, not deleted', () => {
  it('🔴 a stable read drops the experimental target and tab; the stored rows survive', async () => {
    const store = memoryStore();
    await store.save(BACKFILL_TARGETS_KEY, [
      { platform: 'perplexity', origin: PERPLEXITY_ORIGIN, scope: 'acct-w91', at: 1 },
      { platform: 'deepseek', origin: DEEPSEEK_ORIGIN, scope: 'acct-w91', at: 2 },
    ]);
    await store.save(BACKFILL_TABS_KEY, [
      { tabId: 7, origin: PERPLEXITY_ORIGIN, at: 1 },
      { tabId: 8, origin: DEEPSEEK_ORIGIN, at: 2 },
    ]);

    expect((await loadTargets(store, 'stable')).map((t) => t.platform)).toEqual(['deepseek']);
    expect((await loadTabs(store, 'stable')).map((t) => t.origin)).toEqual([DEEPSEEK_ORIGIN]);

    // The dev channel still sees both, and the stable read changed nothing on disk.
    expect((await loadTargets(store, 'dev')).map((t) => t.platform).sort()).toEqual(['deepseek', 'perplexity']);
    expect((await loadTabs(store, 'dev')).map((t) => t.tabId).sort()).toEqual([7, 8]);
    expect((await loadTargets(store, 'dev'))).toHaveLength(2);
    expect((await loadTabs(store, 'dev'))).toHaveLength(2);
  });

  it('🔴 recording a new experimental target/tab in a stable build is a no-op, never a fresh row', async () => {
    const store = memoryStore();
    await expect(rememberTarget(store, {
      platform: 'perplexity', origin: PERPLEXITY_ORIGIN, scope: 'acct-w91', at: 3,
    }, 'stable')).resolves.toEqual([]);
    await expect(rememberTab(store, { tabId: 9, origin: PERPLEXITY_ORIGIN, at: 3 }, 'stable')).resolves.toEqual([]);
    expect(await loadTargets(store, 'dev')).toEqual([]);
    expect(await loadTabs(store, 'dev')).toEqual([]);
  });
});

describe('W91-3 · the engine and the tick refuse an experimental platform in stable', () => {
  it('🔴 runBackfill on an experimental origin halts before any request, naming the table', async () => {
    const store = memoryStore();
    const http = fakeHttp();
    const report = await runBackfill({
      platform: 'perplexity',
      origin: PERPLEXITY_ORIGIN,
      scope: 'acct-w91',
      store,
      http: http.port,
      channel: 'stable',
    });
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('not in the platform table');
    expect(http.calls, 'no request was issued').toEqual([]);
  });

  it('🔴 the same origin in dev, and a stable origin in stable, both pass the platform gate', async () => {
    // The control that proves the halt above is about the channel, not about a
    // missing plan: with `plans: () => null` every surviving origin halts one step
    // later, at `unsupported-platform`.
    for (const opts of [
      { platform: 'perplexity', origin: PERPLEXITY_ORIGIN, channel: 'dev' as const },
      { platform: 'deepseek', origin: DEEPSEEK_ORIGIN, channel: 'stable' as const },
    ]) {
      const http = fakeHttp();
      const report = await runBackfill({
        ...opts,
        scope: 'acct-w91',
        store: memoryStore(),
        http: http.port,
        plans: () => null,
      });
      expect(report.stopped, `${opts.platform}/${opts.channel}`).toBe('halted');
      expect(report.halted?.reason, `${opts.platform}/${opts.channel}`).toBe('unsupported-platform');
      expect(http.calls).toEqual([]);
    }
  });

  it('🔴 a stable tick served an experimental platform directly still issues nothing', async () => {
    const store = memoryStore();
    await setBackfillEnabled(store, true);
    const http = fakeHttp();
    const result = await tickBackfill({
      store,
      platform: 'perplexity',
      origin: PERPLEXITY_ORIGIN,
      scope: 'acct-w91',
      http: http.port,
      host: { paused: async () => false, resume: async () => true },
      channel: 'stable',
    });
    expect(result.ran).toBe(true);
    expect(result.report?.stopped).toBe('halted');
    expect(result.report?.halted?.reason).toBe('shape-changed');
    expect(http.calls, 'the tick issued no request').toEqual([]);
  });
});
