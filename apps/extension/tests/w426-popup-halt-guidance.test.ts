/** W426 · Known capability limits name the unsupported step and an action. */

import { describe, expect, it } from 'vitest';

import { renderPopup, popupText, NO_FAILURES } from '../lib/popup-view';
import type { BackfillHeader, HaltReason } from '../lib/backfill/types';

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

    expect(text).toContain('Backfill is not available for synthetic-platform in this build.');
    expect(text).toContain('open it on synthetic-platform and let live capture run');
    expect(text).toContain('use a supported platform to backfill older conversations');
    expect(text).not.toContain('not a platform change');
    expect(text).not.toContain('no history');
  });

  it('names body fetching as unsupported and says what to do with listed conversations', () => {
    const text = popupFor('detail-unsupported', 'synthetic-platform', 3);

    expect(text).toContain('can list 3 past conversations on synthetic-platform');
    expect(text).toContain('cannot fetch their contents for backfill');
    expect(text).toContain('open it on synthetic-platform and let live capture run');
    expect(text).toContain('the listed conversations remain pending');
    expect(text).not.toContain('nothing has been stored so far');
    expect(text).not.toContain('not a platform change');
  });
});
