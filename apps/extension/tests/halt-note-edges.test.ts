import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CATALOGS, catalogFetch, withI18n, type TestLocale } from './i18n-harness';
import type { HaltRecord } from '../lib/backfill/types';

const NOW = Date.parse('2026-10-04T12:00:00.000Z');

function halt(reason: string, detail = 'synthetic detail', extra: Partial<HaltRecord> = {}): HaltRecord {
  return { reason: reason as HaltRecord['reason'], at: NOW, detail, ...extra };
}

function catalogText(locale: TestLocale, key: string): string {
  return CATALOGS[locale][key.replaceAll('.', '_')]!.message;
}

async function useLocale(locale: TestLocale) {
  const i18n = await import('../lib/i18n');
  await i18n.applyUiLocale(locale);
  const { haltNote } = await import('../lib/halt-note');
  return haltNote;
}

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  const fakeBrowser = withI18n({
    runtime: { id: 'halt-note-edge-test' },
    storage: { local: { async get(query: Record<string, unknown>) { return { ...query }; }, async set() {} } },
  });
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('fetch', catalogFetch());
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe.each(['en', 'zh_CN'] as const)('haltNote (%s)', (locale) => {
  it('returns null when there is no halt record', async () => {
    const haltNote = await useLocale(locale);
    expect(haltNote(null, 'chatgpt', 0, NOW)).toBeNull();
  });

  it('maps unsupported-platform and preserves an empty detail boundary', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('unsupported-platform', ''), 'synthetic-platform', 0, NOW)!;
    const prefix = locale === 'en' ? 'This platform (synthetic-platform)' : 'synthetic-platform';
    expect(note).toContain(prefix);
    expect(note).not.toContain('synthetic detail');
    expect(note).not.toContain('undefined');
    expect(note).toContain(locale === 'en' ? 'Technical detail: ' : '技术细节：');
    expect(note).toContain(catalogText(locale, 'popup.notes.halted.unsupportedPlatform').split('{platform}')[0]!.trim());
  });

  it('maps auth-refused with its own message, interpolation, and rounded retry time', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('auth-refused', 'synthetic auth detail', {
      attempts: 3,
      retryAt: NOW + 60_001,
    }), 'synthetic-platform', 0, NOW)!;
    expect(note).toContain(catalogText(locale, 'popup.notes.halted.authRefused').split('{platform}')[0]!.trim());
    expect(note).toContain('synthetic-platform');
    expect(note).toContain('synthetic auth detail');
    expect(note).toContain(locale === 'en' ? '2 minute(s)' : '2 分钟');
    expect(note).toContain(locale === 'en' ? 'attempt 3' : '第 3 次');
  });

  it('maps refused-unknown separately from auth-refused', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('refused-unknown', 'synthetic code 91', {
      attempts: 2,
      retryAt: NOW + 1,
    }), 'synthetic-platform', 0, NOW)!;
    expect(note).toContain(catalogText(locale, 'popup.notes.halted.refusedUnknown').split('{platform}')[0]!.trim());
    expect(note).toContain('synthetic code 91');
    expect(note).toContain(locale === 'en' ? '1 minute(s)' : '1 分钟');
    expect(note).toContain(locale === 'en' ? 'attempt 2' : '第 2 次');
    expect(note).not.toContain(locale === 'en' ? 'sign in again' : '重新登录');
  });

  it('maps other transient reasons to the waiting sentence and pins retry-minute boundaries', async () => {
    const haltNote = await useLocale(locale);
    const reason = 'transport-error';
    const render = (retryAt?: number) => haltNote(halt(reason, 'synthetic transport detail', {
      attempts: 4,
      ...(retryAt === undefined ? {} : { retryAt }),
    }), 'synthetic-platform', 0, NOW)!;
    const minutePhrase = (minutes: number) => locale === 'en' ? `${minutes} minute(s)` : `${minutes} 分钟`;

    expect(render()).toContain(minutePhrase(0));
    expect(render(NOW - 1)).toContain(minutePhrase(0));
    expect(render(NOW)).toContain(minutePhrase(0));
    expect(render(NOW + 1)).toContain(minutePhrase(1));
    expect(render(NOW + 60_000)).toContain(minutePhrase(1));
    expect(render(NOW + 60_001)).toContain(minutePhrase(2));
    expect(render()).toContain(catalogText(locale, 'popup.notes.halted.waitingRetry').split('{reason}')[0]!.trim());
    expect(render(NOW + 60_001)).toContain('synthetic transport detail');
  });

  it.each([
    ['org-ambiguous', 'chatgpt', 'workspaceAmbiguous'],
    ['org-unresolved', 'chatgpt', 'workspaceUnresolved'],
    ['org-ambiguous', 'claude', 'orgAmbiguous'],
    ['org-unresolved', 'claude', 'orgUnresolved'],
  ])('maps %s on %s to the matching organization sentence', async (reason, platform, key) => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt(reason, 'synthetic resolver detail'), platform, 0, NOW)!;
    expect(note).toContain(catalogText(locale, `popup.notes.halted.${key}`).slice(0, 18).trim());
    expect(note).not.toContain('synthetic resolver detail');
  });

  it('maps account-changed without describing it as an automatic retry', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('account-changed', 'synthetic account detail'), 'claude', 0, NOW)!;
    expect(note).toContain(catalogText(locale, 'popup.notes.halted.accountChanged').split('{detail}')[0]!.trim());
    expect(note).toContain('synthetic account detail');
    expect(note).not.toContain(locale === 'en' ? 'waiting out its backoff' : '退避时间');
  });

  it('maps detail-unsupported and interpolates the pending count, including zero', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('detail-unsupported', 'synthetic detail capability'), 'synthetic-platform', 0, NOW)!;
    expect(note).toContain(catalogText(locale, 'popup.notes.halted.detailUnsupported').split('{platform}')[0]!.trim());
    expect(note).toContain('synthetic-platform');
    expect(note).toContain('0');
    expect(note).toContain('synthetic detail capability');
  });

  it('uses the generic fallback for an unrecognized reason', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('future-reason', 'synthetic future detail'), 'claude', 0, NOW)!;
    expect(note).toContain(catalogText(locale, 'popup.notes.halted.other').split('{reason}')[0]!.trim());
    expect(note).toContain('future-reason');
    expect(note).toContain('synthetic future detail');
  });
});
