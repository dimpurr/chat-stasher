/**
 * W662 — `describeTickReason` maps each tick reason to its plain-sentence catalog string.
 *
 * `describeTickReason` (lib/popup-view.ts:1177) converts the alarm tick's outcome
 * into a single user-visible sentence. The switch handles all ten named `TickReason`
 * values and falls back to a formatted `unknown` entry for unrecognized codes so
 * that an unseen outcome is reported verbatim rather than swallowed.
 *
 * This suite pins:
 * 1. Every named `TickReason` maps to its exact `tick.reason.*` catalog sentence in English.
 * 2. Every named `TickReason` maps to its corresponding catalog sentence in Chinese (zh_CN).
 * 3. All ten named reasons produce distinct sentences in both languages ("Each one must
 *    read differently, or the naming is pointless").
 * 4. The unknown/unrecognized fallback preserves the unrecognized reason verbatim,
 *    substituting into the catalog template in both languages without leaving `{reason}`.
 * 5. Type-level exhaustiveness: `TICK_REASON_KEYS` is typed `Record<TickReason, string>`,
 *    so adding a new `TickReason` to `lib/backfill/schedule.ts` without registering it
 *    here fails at compile time.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { describeTickReason } from '../lib/popup-view';
import type { TickReason } from '../lib/backfill/schedule';
import { applyUiLocale, renderMessage, t } from '../lib/i18n';
import { CATALOGS, withI18n } from './i18n-harness';

/**
 * Mapping of every known TickReason to its catalog key.
 *
 * 🔴 The `Record<TickReason, string>` type annotation ensures compile-time exhaustiveness:
 *    a new `TickReason` added to the union must be explicitly cataloged here.
 */
const TICK_REASON_KEYS: Record<TickReason, string> = {
  'no-targets': 'tick.reason.noTargets',
  'no-http-port': 'tick.reason.noHttpPort',
  'no-runnable-target': 'tick.reason.noRunnableTarget',
  'scope-asked': 'tick.reason.scopeAsked',
  disabled: 'tick.reason.disabled',
  'no-store': 'tick.reason.noStore',
  'host-paused': 'tick.reason.hostPaused',
  'outbox-near-full': 'tick.reason.outboxNearFull',
  'already-running': 'tick.reason.alreadyRunning',
  ran: 'tick.reason.ran',
};

const ALL_TICK_REASONS = Object.keys(TICK_REASON_KEYS) as TickReason[];

function chromeKey(key: string): string {
  return key.replaceAll('.', '_');
}

beforeEach(() => {
  vi.stubGlobal('browser', withI18n({} as never));
});

afterEach(async () => {
  await applyUiLocale('auto');
});

describe('W662 · describeTickReason English mappings', () => {
  it.each(ALL_TICK_REASONS)('%s maps to its own catalog entry', (reason) => {
    const key = TICK_REASON_KEYS[reason];
    expect(describeTickReason(reason)).toBe(t(key));
    expect(describeTickReason(reason)).toBe(CATALOGS.en[chromeKey(key)]!.message);
  });

  it('all ten named reasons produce distinct descriptions', () => {
    const descriptions = ALL_TICK_REASONS.map(describeTickReason);
    expect(new Set(descriptions).size).toBe(ALL_TICK_REASONS.length);
  });

  it('every named reason yields a non-empty string without placeholders', () => {
    for (const reason of ALL_TICK_REASONS) {
      const desc = describeTickReason(reason);
      expect(typeof desc).toBe('string');
      expect(desc.trim().length, `${reason} was empty`).toBeGreaterThan(0);
      expect(desc).not.toContain('{');
    }
  });

  it('covers exactly the ten known TickReason variants', () => {
    expect(ALL_TICK_REASONS).toHaveLength(10);
    expect(ALL_TICK_REASONS.sort()).toEqual([
      'already-running',
      'disabled',
      'host-paused',
      'no-http-port',
      'no-runnable-target',
      'no-store',
      'no-targets',
      'outbox-near-full',
      'ran',
      'scope-asked',
    ].sort());
  });
});

describe('W662 · describeTickReason zh_CN catalog mappings', () => {
  it.each(ALL_TICK_REASONS)('%s maps to its own zh_CN catalog entry', async (reason) => {
    await applyUiLocale('zh_CN');
    const key = TICK_REASON_KEYS[reason];
    expect(describeTickReason(reason)).toBe(CATALOGS.zh_CN[chromeKey(key)]!.message);
  });

  it('all ten zh_CN descriptions are distinct and none falls back to English', async () => {
    await applyUiLocale('zh_CN');
    const descriptions = ALL_TICK_REASONS.map(describeTickReason);
    expect(new Set(descriptions).size).toBe(ALL_TICK_REASONS.length);

    for (const reason of ALL_TICK_REASONS) {
      const key = TICK_REASON_KEYS[reason];
      const zh = describeTickReason(reason);
      const en = CATALOGS.en[chromeKey(key)]!.message;
      expect(zh, `${reason} fell back to English`).not.toBe(en);
      expect(zh).not.toContain('{');
      expect(zh.trim().length).toBeGreaterThan(0);
    }
  });
});

describe('W662 · describeTickReason unknown / unrecognized fallback', () => {
  it('reports unrecognized reason codes verbatim in English', () => {
    expect(describeTickReason('unrecognized-code')).toBe('outcome code: unrecognized-code');
    expect(describeTickReason('future-variant-v2')).toBe('outcome code: future-variant-v2');
    expect(describeTickReason('')).toBe('outcome code: ');
    expect(describeTickReason('syntax error: 404')).toBe('outcome code: syntax error: 404');
  });

  it('reports unrecognized reason codes verbatim in zh_CN', async () => {
    await applyUiLocale('zh_CN');
    const zhTemplate = CATALOGS.zh_CN.tick_reason_unknown!.message;
    expect(describeTickReason('unrecognized-code')).toBe(renderMessage(zhTemplate, [{ reason: 'unrecognized-code' }]));
    expect(describeTickReason('future-variant-v2')).toBe(renderMessage(zhTemplate, [{ reason: 'future-variant-v2' }]));
    expect(describeTickReason('')).toBe(renderMessage(zhTemplate, [{ reason: '' }]));
  });

  it('never leaves {reason} placeholder in the formatted output', () => {
    for (const code of ['foo', 'bar', 'custom-outcome']) {
      const desc = describeTickReason(code);
      expect(desc).not.toContain('{reason}');
      expect(desc).toContain(code);
    }
  });
});
