/** W475 · Persisted hook-report declines give distinct, safe popup guidance. */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { applyUiLocale } from '../lib/i18n';
import { NO_FAILURES, renderPopup, type PopupModel } from '../lib/popup-view';
import {
  HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN,
  HOOK_DECLINE_UNREADABLE_MESSAGE,
} from '../lib/hook-status';
import { catalogFetch, withI18n } from './i18n-harness';

const ORIGIN = 'https://synthetic.example';
const PATH = '/private/report-path';
const BODY = 'synthetic report body';
const TOKEN = 'synthetic-secret-token';
const PRIVATE_FIELDS = { url: `${ORIGIN}${PATH}`, message: BODY, token: TOKEN };
const AT = Date.parse('2026-10-04T12:00:00.000Z');

function model(hookDecline: NonNullable<PopupModel['hookDecline']>): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    hookDecline,
  };
}

beforeEach(async () => {
  vi.stubGlobal('browser', withI18n({} as never));
  vi.stubGlobal('fetch', catalogFetch());
  await applyUiLocale('auto');
});

describe('W475 · popup guidance for persisted hook-report declines', () => {
  it.each(['en', 'zh_CN'] as const)('keeps the two observations distinct in %s', async (locale) => {
    await applyUiLocale(locale);

    const unsupportedOrigin = renderPopup(model({
      ...PRIVATE_FIELDS,
      at: AT,
      count: 2,
      reason: HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN,
      origin: ORIGIN,
      observation: null,
    })).notes.join('\n');
    const unreadableMessage = renderPopup(model({
      ...PRIVATE_FIELDS,
      at: AT,
      count: 3,
      reason: HOOK_DECLINE_UNREADABLE_MESSAGE,
      origin: null,
      observation: null,
    })).notes.join('\n');

    expect(unsupportedOrigin).toContain(ORIGIN);
    expect(unsupportedOrigin).toContain(locale === 'en' ? 'sent a live-capture report' : '发来了一条实时抓取报告');
    expect(unsupportedOrigin).toContain(locale === 'en' ? 'received as of' : '报告送达时间');
    expect(unsupportedOrigin).toContain(locale === 'en' ? 'supported platform' : '支持的平台');
    expect(unreadableMessage).toContain(locale === 'en' ? 'could not read' : '读不懂');
    expect(unreadableMessage).toContain(locale === 'en' ? 'received as of' : '报告送达时间');
    expect(unreadableMessage).toContain(locale === 'en' ? 'Reload the page' : '刷新页面');
    expect(unreadableMessage).not.toContain(ORIGIN);
    expect(unsupportedOrigin).not.toBe(unreadableMessage);

    for (const rendered of [unsupportedOrigin, unreadableMessage]) {
      expect(rendered).not.toContain(PATH);
      expect(rendered).not.toContain(BODY);
      expect(rendered).not.toContain(TOKEN);
      expect(rendered).not.toContain(locale === 'en' ? 'not being archived' : '不会被归档');
      expect(rendered).not.toContain(locale === 'en' ? 'capture failed' : '抓取失败');
    }
    expect(unsupportedOrigin).not.toContain(locale === 'en' ? 'install a capture hook' : '装抓取钩子');
  });
});
