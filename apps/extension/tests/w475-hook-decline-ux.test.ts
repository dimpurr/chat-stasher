/** W475 · Persisted hook-report declines give distinct, safe popup guidance. */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { applyUiLocale } from '../lib/i18n';
import { NO_FAILURES, renderPopup, type PopupModel } from '../lib/popup-view';
import {
  HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN,
  HOOK_DECLINE_UNREADABLE_MESSAGE,
} from '../lib/hook-status';
import { CATALOGS, catalogFetch, withI18n } from './i18n-harness';

const ORIGIN = 'https://synthetic.example';
const PATH = '/private/report-path';
const BODY = 'synthetic report body';
const TOKEN = 'synthetic-secret-token';
const PRIVATE_FIELDS = { url: `${ORIGIN}${PATH}`, message: BODY, token: TOKEN };
const AT = Date.parse('2026-10-04T12:00:00.000Z');

function compiledDeclineMessagePattern(
  kind: 'origin' | 'message',
  locale: 'en' | 'zh_CN',
  count: string,
): RegExp {
  const suffix = kind === 'origin' ? 'declinedOrigin' : 'declinedMessage';
  const entry = Object.entries(CATALOGS[locale]).find(([key]) => key.endsWith(suffix))?.[1];
  if (!entry) throw new Error(`compiled ${locale} catalog is missing the ${kind} decline message`);
  const escaped = entry.message.split(/(\{(?:origin|when|count)\})/g)
    .map((part) => part.startsWith('{') ? (part === '{count}' ? count : '.+?') : part.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
    .join('');
  return new RegExp(escaped);
}

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

    expect(unsupportedOrigin).toMatch(compiledDeclineMessagePattern('origin', locale, '2'));
    expect(unreadableMessage).toMatch(compiledDeclineMessagePattern('message', locale, '3'));

    expect(unsupportedOrigin).toContain(ORIGIN);
    if (locale === 'en') {
      expect(unsupportedOrigin).toContain('sent a live-capture report');
      expect(unsupportedOrigin).toContain('received as of');
      expect(unsupportedOrigin).toContain('supported platform');
      expect(unreadableMessage).toContain('could not read');
      expect(unreadableMessage).toContain('received as of');
      expect(unreadableMessage).toContain('Reload the page');
    }
    expect(unreadableMessage).not.toContain(ORIGIN);
    expect(unsupportedOrigin).not.toBe(unreadableMessage);

    for (const rendered of [unsupportedOrigin, unreadableMessage]) {
      expect(rendered).not.toContain(PATH);
      expect(rendered).not.toContain(BODY);
      expect(rendered).not.toContain(TOKEN);
      if (locale === 'en') {
        expect(rendered).not.toContain('not being archived');
        expect(rendered).not.toContain('capture failed');
      }
    }
    if (locale === 'en') expect(unsupportedOrigin).not.toContain('install a capture hook');
  });
});
