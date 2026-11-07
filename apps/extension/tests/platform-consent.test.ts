import { describe, expect, it } from 'vitest';
import {
  CONSENT_KEY,
  migrateLegacyConsent,
  type PlatformConsentRecord,
} from '../lib/platform-consent';
import { outboxEntryBelongsToPlatform } from '../lib/outbox';
import { platformLocalKeyPlan } from '../lib/platform-cleanup';
import { memoryStore } from '../lib/backfill/store';
import { resetTickLockForTest, tickBackfill } from '../lib/backfill/schedule';
import { IDBFactory } from 'fake-indexeddb';
import { clearPlatformLocalData } from '../lib/platform-cleanup';
import { enqueue, listEntries, resetOutboxConnectionForTest } from '../lib/outbox';
import { resetDebtDbConnectionForTest } from '../lib/backfill/debt-store';

describe('per-platform consent migration', () => {
  it('migrates an explicit legacy false to every platform off', () => {
    const migrated = migrateLegacyConsent(false, ['chatgpt', 'claude'], 1234);
    expect(migrated).toEqual({
      key: CONSENT_KEY,
      value: {
        version: 1,
        platforms: {
          chatgpt: { state: 'off', updatedAt: 1234 },
          claude: { state: 'off', updatedAt: 1234 },
        },
      } satisfies PlatformConsentRecord,
    });
  });

  it.each([null, true, 'invalid'])('migrates legacy value %s to every platform on', (legacy) => {
    const migrated = migrateLegacyConsent(legacy, ['chatgpt', 'claude'], 5678);
    expect(Object.values(migrated.value.platforms).map((entry) => entry.state)).toEqual(['on', 'on']);
  });
});

describe('platform outbox matching', () => {
  it('matches by payload platform or exact platform delivery-name prefix', () => {
    expect(outboxEntryBelongsToPlatform({ name: 'other.json', payload: '{"platform":"claude"}' }, 'claude')).toBe(true);
    expect(outboxEntryBelongsToPlatform({ name: 'claude-session.json', payload: '{}' }, 'claude')).toBe(true);
    expect(outboxEntryBelongsToPlatform({ name: 'claude-extra.json', payload: '{"platform":"chatgpt"}' }, 'claude')).toBe(false);
    expect(outboxEntryBelongsToPlatform({ name: 'xclaude-session.json', payload: '{}' }, 'claude')).toBe(false);
  });
});

describe('platform local cleanup inventory', () => {
  it('removes this platform rows and edits shared registries without touching other platforms', () => {
    expect(platformLocalKeyPlan([
      'cs_backfill_v2:claude:scope-a',
      'cs_backfill_v2:chatgpt:scope-b',
      'cs_backfill_v1:claude:legacy-scope',
      'cs_live_capture_v1:claude',
      'cs_hook_v1:https://claude.ai',
      'cs_hook_v1:https://chatgpt.com',
      'cs_consent_refusals_v1:claude',
      'cs_backfill_targets_v1',
      'cs_backfill_day_slow_v1',
      'cs_backfill_tabs_v1',
      'cs_backfill_cursor_v1',
    ], 'claude', ['https://claude.ai'])).toEqual({
      remove: [
        'cs_backfill_v2:claude:scope-a',
        'cs_backfill_v1:claude:legacy-scope',
        'cs_live_capture_v1:claude',
        'cs_hook_v1:https://claude.ai',
        'cs_consent_refusals_v1:claude',
      ],
      edit: ['cs_backfill_targets_v1', 'cs_backfill_day_slow_v1', 'cs_backfill_tabs_v1', 'cs_backfill_cursor_v1'],
    });
  });

  it('clears the selected platform outbox and local state while retaining another platform', async () => {
    (globalThis as { indexedDB?: IDBFactory }).indexedDB = new IDBFactory();
    resetOutboxConnectionForTest();
    resetDebtDbConnectionForTest();
    const store = memoryStore({
      cs_backfill_targets_v1: [
        { platform: 'claude', origin: 'https://claude.ai', scope: 'one', at: 1 },
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'two', at: 2 },
      ],
      'cs_backfill_v2:claude:one': { platform: 'claude', scope: 'one' },
      'cs_backfill_v2:chatgpt:two': { platform: 'chatgpt', scope: 'two' },
      cs_backfill_day_slow_v1: { v: 1, platforms: { claude: { until: 1, at: 1 }, chatgpt: { until: 2, at: 2 } } },
      cs_backfill_tabs_v1: [
        { tabId: 1, origin: 'https://claude.ai', at: 1 },
        { tabId: 2, origin: 'https://chatgpt.com', at: 2 },
      ],
      cs_backfill_cursor_v1: { served: { 'claude\u0000one': 1, 'chatgpt\u0000two': 2 } },
      'cs_live_capture_v1:claude': { platform: 'claude', at: 1, count: 1 },
      'cs_hook_v1:https://claude.ai': { origin: 'https://claude.ai' },
    });
    expect((await enqueue('claude-one.json', '{"platform":"claude"}')).accepted).toBe(true);
    expect((await enqueue('chatgpt-two.json', '{"platform":"chatgpt"}')).accepted).toBe(true);
    const result = await clearPlatformLocalData(store, 'claude', ['https://claude.ai']);
    expect(result.removedOutbox).toBe(1);
    expect(result.removedKeys).toContain('cs_backfill_v2:claude:one');
    expect(result.removedKeys).toContain('cs_live_capture_v1:claude');
    expect(result.removedKeys).toContain('cs_hook_v1:https://claude.ai');
    expect(store.data.cs_backfill_targets_v1).toEqual([
      { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'two', at: 2 },
    ]);
    expect(store.data['cs_live_capture_v1:claude']).toBeUndefined();
    expect(store.data['cs_backfill_v2:chatgpt:two']).toBeDefined();
    expect((store.data.cs_backfill_day_slow_v1 as { platforms: Record<string, unknown> }).platforms)
      .toEqual({ chatgpt: { until: 2, at: 2 } });
    expect(store.data.cs_backfill_tabs_v1).toEqual([
      { tabId: 2, origin: 'https://chatgpt.com', at: 2 },
    ]);
    expect(store.data.cs_backfill_cursor_v1).toEqual({ served: { 'chatgpt\u0000two': 2 } });
    expect(await listEntries()).toHaveLength(1);
    expect(JSON.parse((await listEntries())![0]!.payload).platform).toBe('chatgpt');
  });
});

describe('platform-disabled backfill gate', () => {
  it('returns before host checks or requests for an explicitly off platform', async () => {
    resetTickLockForTest();
    const store = memoryStore({
      cs_platform_consent_v1: {
        version: 1,
        platforms: { chatgpt: { state: 'off', updatedAt: 1234 } },
      },
    });
    let hostChecks = 0;
    const result = await tickBackfill({
      store,
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'default',
      host: {
        paused: async () => { hostChecks += 1; return false; },
        resume: async () => { hostChecks += 1; return true; },
      },
    });
    expect(result).toEqual({ ran: false, reason: 'platform-disabled', report: null });
    expect(hostChecks).toBe(0);
  });
});
