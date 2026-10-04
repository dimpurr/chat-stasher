/** W426 · Known capability limits name the unsupported step and an action. */

import { describe, expect, it } from 'vitest';

import { renderPopup, popupText, NO_FAILURES } from '../lib/popup-view';
import type { BackfillHeader, HaltReason } from '../lib/backfill/types';
import { CATALOGS } from './i18n-harness';

const T0 = Date.parse('2026-10-04T09:00:00.000Z');

function popupFor(reason: HaltReason, platform: string, pendingCount: number) {
  const state: BackfillHeader = {
    v: 2,
    platform,
    scope: 'w426-synthetic-scope',
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pendingCount,
    archivedCount: 0,
    detailToday: { day: '2026-10-04', count: 0 },
    halted: { reason, at: T0, detail: 'synthetic fixture detail' },
  };
  return popupText(renderPopup({
    enabled: true,
    block: null,
    state,
    target: { platform, scope: state.scope },
    failures: NO_FAILURES,
    now: T0,
  }));
}

describe('W426 · popup guidance for known capability limits', () => {
  it('says platform backfill is unsupported and gives a live-capture next step', () => {
    const text = popupFor('unsupported-platform', 'synthetic-platform', 0);

    expect(text).toContain('Backfill is not available for synthetic-platform in this build');
    expect(text).toContain('can only be backfilled from that platform');
    expect(text).toContain('open it on synthetic-platform and let live capture run');
    // Backfill is scoped to its own platform and account, so no other platform
    // can backfill this one's history — the guidance must not send the user to
    // a supported platform as though it could.
    expect(text).not.toContain('use a supported platform');
    expect(text).not.toContain('not a platform change');
    expect(text).not.toContain('no history');
  });

  it('names body fetching as unsupported and says what to do with listed conversations', () => {
    const text = popupFor('detail-unsupported', 'synthetic-platform', 3);

    expect(text).toContain('can list past conversations on synthetic-platform');
    expect(text).toContain('cannot fetch their contents for backfill');
    expect(text).toContain('3 remain pending');
    expect(text).toContain('open it on synthetic-platform and let live capture run');
    // {pending} is the count still waiting, not the number listable: the
    // caller's pendingCount excludes already archived conversations.
    expect(text).not.toContain('can list 3 past conversations');
    expect(text).not.toContain('nothing has been stored so far');
    expect(text).not.toContain('not a platform change');
  });

  it('with zero pending it states no total and implies no empty history', () => {
    const text = popupFor('detail-unsupported', 'synthetic-platform', 0);

    expect(text).toContain('can list past conversations on synthetic-platform');
    expect(text).toContain('0 remain pending');
    expect(text).not.toContain('can list 0 past conversations');
    expect(text).not.toContain('no past conversations');
  });

  it('says the same guidance in Chinese', async () => {
    const overlay = await import('../lib/i18n');
    // English baselines first: the locale switch below is process-wide state.
    const en = popupFor('unsupported-platform', 'synthetic-platform', 0);
    const enDetail = popupFor('detail-unsupported', 'synthetic-platform', 3);
    await overlay.applyUiLocale('zh_CN');
    try {
      const zh = popupFor('unsupported-platform', 'synthetic-platform', 0);
      // The whole entry, exactly as the catalog writes it: a missing zh_CN entry
      // (or a silent fallback to English) cannot satisfy this.
      expect(zh).toContain(CATALOGS.zh_CN.popup_notes_halted_unsupportedPlatform!.message
        .replaceAll('{platform}', 'synthetic-platform')
        .replaceAll('{detail}', 'synthetic fixture detail'));
      expect(zh).not.toBe(en);

      const zhDetail = popupFor('detail-unsupported', 'synthetic-platform', 3);
      expect(zhDetail).toContain(CATALOGS.zh_CN.popup_notes_halted_detailUnsupported!.message
        .replaceAll('{platform}', 'synthetic-platform')
        .replaceAll('{pending}', '3')
        .replaceAll('{detail}', 'synthetic fixture detail'));
      expect(zhDetail).not.toBe(enDetail);
    } finally {
      await overlay.applyUiLocale('auto');
    }
  });
});
