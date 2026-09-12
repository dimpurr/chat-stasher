/**
 * Switching the popup's language really switches the popup.
 *
 * The overlay being correct is not the same as the popup being translated: the
 * render layer could still hold a string captured at import time, and every
 * overlay test would stay green while the screen never changed. So this renders
 * the **same model** twice, in two languages, and requires the text to differ in
 * the areas a user actually reads — the channel, the outbox and the export
 * button — and to match the catalog in each case.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { CATALOGS, withI18n } from './i18n-harness';
import { NO_FAILURES, renderPopup, popupText, type PopupModel } from '../lib/popup-view';

const AT = Date.parse('2026-09-12T21:47:03.000Z');

const fakeBrowser = withI18n({
  runtime: { id: 'i18n-popup-test' },
  storage: {
    local: {
      async get(query: Record<string, unknown>) { return { ...query }; },
      async set() { /* nothing to persist in this file */ },
    },
  },
});

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
});

afterEach(() => {
  vi.restoreAllMocks();
});

/** One model exercising all three areas under test at once. */
function model(): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    nativeHost: {
      at: AT, ok: false, reason: 'timeout', lastKnownStage: '/Users/me/stage',
    },
    outbox: {
      pending: 3, rejected: 1, bytes: 1024 * 1024, capacityBytes: 256 * 1024 * 1024,
      full: false,
      rejectedKinds: [{ kind: 'config', count: 1 }],
      rejectedSamples: [{ kind: 'config', detail: 'nack:config' }],
    },
    lastExport: { at: AT, entries: 4, bytes: 2048, filename: 'chat-stasher-export-20260912T214703Z.jsonl' },
  };
}

async function rendered() {
  const overlay = await import('../lib/i18n');
  const view = await import('../lib/popup-view');
  return { overlay, view };
}

describe('i18n popup · the language switch changes what is on screen', () => {
  it('the same model renders Chinese under zh_CN and English under en', async () => {
    const { overlay, view } = await rendered();

    await overlay.applyUiLocale('zh_CN');
    const zh = view.renderPopup(model());
    await overlay.applyUiLocale('en');
    const en = view.renderPopup(model());

    // The three areas the task names, plus the flattened whole.
    const areas = ['channel', 'outbox', 'exportFile'] as const;
    for (const area of areas) {
      const a = area === 'exportFile' ? zh.exportFile.label : zh[area];
      const b = area === 'exportFile' ? en.exportFile.label : en[area];
      expect(a, `${area} did not change language`).not.toBe(b);
      expect(typeof a).toBe('string');
      expect(typeof b).toBe('string');
    }

    // …and each one matches the catalog it claims to be speaking.
    expect(zh.channel).toContain(CATALOGS.zh_CN.channel_disconnected_head!.message
      .replace('{why}', 'timeout')
      .replace('{at}', '2026-09-12 21:47:03 UTC'));
    expect(zh.channel).toContain(CATALOGS.zh_CN.channel_disconnected_stageKnown!.message
      .replace('{stage}', '/Users/me/stage'));
    expect(zh.outbox).toContain(CATALOGS.zh_CN.outbox_head!.message
      .replace('{pending}', '3').replace('{rejected}', '1')
      .replace('{bytes}', '1.0 MiB').replace('{capacity}', '256.0 MiB'));
    expect(zh.exportFile.label).toBe(CATALOGS.zh_CN.export_buttonLabel!.message);

    expect(en.exportFile.label).toBe(CATALOGS.en.export_buttonLabel!.message);
    expect(en.outbox).toContain('Outbox: 3 waiting, 1 rejected');
    expect(en.channel).toContain('NOT connected');

    // The flattened text switches with it, so a whole-screen regression cannot hide.
    expect(popupText(zh)).not.toBe(popupText(en));
    expect(popupText(zh)).toContain(CATALOGS.zh_CN.export_buttonLabel!.message);
    expect(popupText(en)).toContain(CATALOGS.en.export_buttonLabel!.message);
  });

  it('the selector reports the current value and labels every option in its own script', async () => {
    const { overlay, view } = await rendered();
    await overlay.applyUiLocale('zh_CN');
    const zh = view.renderPopup(model()).locale;
    expect(zh.value).toBe('zh_CN');
    expect(zh.label).toBe(CATALOGS.zh_CN.popup_locale_label!.message);
    expect(zh.options.map((o) => o.value)).toEqual(['auto', 'en', 'zh_CN']);
    // Each option is written in its own language, in either UI language.
    expect(zh.options.map((o) => o.label)).toEqual([
      CATALOGS.en.popup_locale_auto!.message,
      CATALOGS.en.popup_locale_english!.message,
      CATALOGS.en.popup_locale_chinese!.message,
    ]);
    await overlay.applyUiLocale('en');
    expect(view.renderPopup(model()).locale.options.map((o) => o.label))
      .toEqual(zh.options.map((o) => o.label));
  });

  it('under auto the popup follows the browser, and says so', async () => {
    vi.stubGlobal('browser', withI18n({ runtime: { id: 'i18n-popup-zh' } }, 'zh_CN'));
    vi.resetModules();
    const { overlay, view } = await rendered();
    expect(overlay.currentUiLocale()).toBe('auto');
    const v = view.renderPopup(model());
    expect(v.locale.value).toBe('auto');
    expect(v.exportFile.label).toBe(CATALOGS.zh_CN.export_buttonLabel!.message);
  });

  it('a language switch reaches text that only appears in a non-healthy state', async () => {
    // The cold-start guidance is the longest block in the popup and the one a
    // user most needs to read; it has to translate like everything else.
    const { overlay, view } = await rendered();
    const cold: PopupModel = {
      enabled: true, block: 'no-targets', state: null, target: null,
      failures: NO_FAILURES, liveTarget: { platform: 'chatgpt', origin: 'https://chatgpt.com' },
      targetCount: 0,
    };
    await overlay.applyUiLocale('en');
    const en = view.renderPopup(cold);
    await overlay.applyUiLocale('zh_CN');
    const zh = view.renderPopup(cold);
    expect(en.startBackfill.label).toBe(CATALOGS.en.popup_startBackfill_label!.message);
    expect(zh.startBackfill.label).toBe(CATALOGS.zh_CN.popup_startBackfill_label!.message);
    expect(en.missing).not.toBe(zh.missing);
    expect(popupText(en)).not.toContain(CATALOGS.zh_CN.popup_missing_noTargets_body!.message);
    expect(popupText(zh)).toContain(CATALOGS.zh_CN.popup_missing_noTargets_body!.message);
  });
});
