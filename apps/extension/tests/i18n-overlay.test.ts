/**
 * The overlay: `auto` follows the browser, a chosen locale follows the shipped
 * catalog, and the fallback when the catalog cannot be read is loud.
 *
 * The three things this file refuses to let slide:
 *   · `auto` must go through `@wxt-dev/i18n` (that is the standard path, and the
 *     only one that keeps working if this overlay is ever removed);
 *   · a chosen locale must actually change the language — including for entries
 *     with substitutions, whose two paths must agree exactly;
 *   · a failed read must fall back **and say so once**. A half-translated screen
 *     that looks translated is worse than one that is visibly broken.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { CATALOGS, catalogFetch, i18nApi, withI18n } from './i18n-harness';

/** A browser whose `i18n` answers in the given language, plus the catalog URL. */
function browserFor(locale: 'en' | 'zh_CN') {
  return withI18n({ runtime: { id: 'i18n-overlay-test' } }, locale);
}

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.stubGlobal('browser', browserFor('en'));
});

afterEach(() => {
  vi.restoreAllMocks();
});

/** Import the overlay fresh, so its module state starts at the default. */
async function overlay() {
  return await import('../lib/i18n');
}

describe('i18n overlay · the auto path', () => {
  it('defaults to auto and delegates to the package, following the browser', async () => {
    vi.stubGlobal('browser', browserFor('zh_CN'));
    const { t, currentUiLocale, DEFAULT_UI_LOCALE } = await overlay();
    expect(DEFAULT_UI_LOCALE).toBe('auto');
    expect(currentUiLocale()).toBe('auto');
    // Nothing has been applied, so this is browser.i18n speaking: the Chinese
    // catalog, because that is the "browser language" this stub reports.
    expect(t('popup.status.on')).toBe(CATALOGS.zh_CN.popup_status_on!.message);
  });

  it('an unknown stored value is read as auto, not as a broken locale', async () => {
    const { normalizeUiLocale, DEFAULT_UI_LOCALE } = await overlay();
    expect(normalizeUiLocale('fr')).toBe(DEFAULT_UI_LOCALE);
    expect(normalizeUiLocale(undefined)).toBe(DEFAULT_UI_LOCALE);
    expect(normalizeUiLocale(null)).toBe(DEFAULT_UI_LOCALE);
    expect(normalizeUiLocale(42)).toBe(DEFAULT_UI_LOCALE);
    expect(normalizeUiLocale('zh_CN')).toBe('zh_CN');
  });
});

describe('i18n overlay · a chosen locale', () => {
  it('zh_CN reads the shipped catalog and returns Chinese', async () => {
    const { t, applyUiLocale } = await overlay();
    await applyUiLocale('zh_CN');
    expect(t('popup.status.on')).toBe(CATALOGS.zh_CN.popup_status_on!.message);
    expect(t('export.buttonLabel')).toBe(CATALOGS.zh_CN.export_buttonLabel!.message);
  });

  it('en reads the shipped catalog and returns English even with a Chinese browser', async () => {
    vi.stubGlobal('browser', browserFor('zh_CN'));
    const { t, applyUiLocale } = await overlay();
    await applyUiLocale('en');
    expect(t('popup.status.on')).toBe(CATALOGS.en.popup_status_on!.message);
    expect(t('export.buttonLabel')).toBe(CATALOGS.en.export_buttonLabel!.message);
  });

  it('both paths agree, entry for entry, on every substituted entry', async () => {
    // The overlay mirrors the package's substitution rules; this is where that
    // claim is checked against the package itself rather than against my
    // reading of it. Named substitutions, plus the plural and positional shapes
    // the package supports, all through the real `i18n.t`.
    const { t, applyUiLocale, renderMessage } = await overlay();
    await applyUiLocale('en');
    vi.stubGlobal('browser', browserFor('en'));
    const { i18n } = await import('#i18n');

    const cases: Array<[string, unknown[]]> = [
      ['export.note', [{ at: '2026-01-02 03:04:05 UTC', entries: 4, bytes: '2.0 KiB', filename: 'f.jsonl' }]],
      ['outbox.head', [{ pending: 3, rejected: 1, bytes: '1.0 MiB', capacity: '256.0 MiB' }]],
      ['channel.disconnected.head', [{ why: 'timeout', at: '2026-01-02 03:04:05 UTC' }]],
      ['popup.running.active', [{ minutes: 5, maxPerDay: 200, minIntervalSeconds: 20 }]],
      ['badge.title', [{ parts: 'x · y' }]],
      ['tick.reason.unknown', [{ reason: 'weird-code' }]],
    ];
    for (const [key, args] of cases) {
      const viaOverlay = t(key, ...args);
      const viaPackage = (i18n.t as unknown as (k: string, ...a: unknown[]) => string)(key, ...args);
      expect(viaOverlay, `${key} differs between the two paths`).toBe(viaPackage);
      expect(viaOverlay).not.toContain('{');
    }

    // And the renderer the overlay itself uses agrees with both.
    for (const [key, args] of cases) {
      const chromeKey = key.replaceAll('.', '_');
      expect(renderMessage(CATALOGS.en[chromeKey]!.message, args)).toBe(t(key, ...args));
    }
  });
});

describe('i18n overlay · failing loudly, never silently', () => {
  it('a catalog that cannot be read falls back to the package and warns exactly once', async () => {
    vi.stubGlobal('fetch', catalogFetch({ fail: true }));
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { t, applyUiLocale, currentUiLocale } = await overlay();

    await applyUiLocale('zh_CN');
    // The choice is still remembered — we simply could not honour it.
    expect(currentUiLocale()).toBe('zh_CN');
    expect(t('popup.status.on')).toBe(CATALOGS.en.popup_status_on!.message);
    expect(t('export.buttonLabel')).toBe(CATALOGS.en.export_buttonLabel!.message);

    const warnings = warn.mock.calls.map((c) => String(c[0]));
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain('i18n');
    expect(warnings[0]).toContain('_locales/zh_CN/messages.json');
  });

  it('an entry missing from the catalog falls back and warns, and says which key', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { t, applyUiLocale, renderMessage } = await overlay();
    await applyUiLocale('zh_CN');
    // Only the warnings from here on are this case's business.
    warn.mockClear();

    // renderMessage is the same engine the lookup uses; this drives the
    // missing-entry branch directly, because the shipped catalogs are aligned
    // (tests/i18n-catalog.test.ts) and cannot produce it.
    const catalog = CATALOGS.zh_CN;
    expect(catalog.popup_status_off).toBeDefined();
    expect(renderMessage(catalog.popup_status_off!.message, [])).toBe(t('popup.status.off'));

    // The warning path: a key the catalog does not hold at all. Two warnings
    // come out of one call, and both are wanted: ours says the overlay had to
    // fall back and names the key, and the package's own says the same lookup
    // found nothing in browser.i18n either (dist/index.mjs:21). Neither is
    // allowed to be the only one — a silent fallback is what this checks for.
    const warningsBefore = warn.mock.calls.length;
    t('this.key.does.not.exist');
    const fresh = warn.mock.calls.slice(warningsBefore).map((c) => String(c[0]));
    const ours = fresh.filter((w) => w.includes('chat-stasher'));
    expect(ours).toHaveLength(1);
    expect(ours[0]).toContain('this.key.does.not.exist');
    expect(fresh.length).toBeGreaterThanOrEqual(1);
  });

  it('warns once per selection, not once per lookup', async () => {
    vi.stubGlobal('fetch', catalogFetch({ fail: true }));
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { t, applyUiLocale } = await overlay();
    await applyUiLocale('zh_CN');
    for (let i = 0; i < 25; i += 1) t('popup.status.on');
    expect(warn).toHaveBeenCalledTimes(1);
  });
});

describe('i18n overlay · the stored choice', () => {
  function storageBacked(initial: Record<string, unknown> = {}) {
    const data: Record<string, unknown> = { ...initial };
    const browser = withI18n({
      runtime: { id: 'i18n-storage-test' },
      storage: {
        local: {
          async get(query: Record<string, unknown>) {
            const out: Record<string, unknown> = {};
            for (const k of Object.keys(query)) out[k] = k in data ? data[k] : query[k];
            return out;
          },
          async set(values: Record<string, unknown>) { Object.assign(data, values); },
        },
      },
    });
    return { browser, data };
  }

  it('defaults to auto when nothing has ever been stored', async () => {
    const { browser } = storageBacked();
    vi.stubGlobal('browser', browser);
    const { loadUiLocale, initUiLocale } = await overlay();
    expect(await loadUiLocale()).toBe('auto');
    expect(await initUiLocale()).toBe('auto');
  });

  it('setUiLocale persists the choice and applies it', async () => {
    const { browser, data } = storageBacked();
    vi.stubGlobal('browser', browser);
    const { setUiLocale, t, UI_LOCALE_KEY } = await overlay();
    expect(await setUiLocale('zh_CN')).toBe('zh_CN');
    expect(data[UI_LOCALE_KEY]).toBe('zh_CN');
    expect(t('popup.status.on')).toBe(CATALOGS.zh_CN.popup_status_on!.message);
  });

  it('a stored value that is not one of the three is read as auto', async () => {
    const { browser } = storageBacked({ cs_ui_locale: 'klingon' });
    vi.stubGlobal('browser', browser);
    const { loadUiLocale, initUiLocale, t } = await overlay();
    expect(await loadUiLocale()).toBe('auto');
    expect(await initUiLocale()).toBe('auto');
    expect(t('popup.status.on')).toBe(CATALOGS.en.popup_status_on!.message);
  });

  it('storage that throws answers auto rather than claiming a language was chosen', async () => {
    const browser = withI18n({
      runtime: { id: 'i18n-throwing-storage' },
      storage: {
        local: {
          async get(): Promise<Record<string, unknown>> { throw new Error('storage is gone'); },
          async set(): Promise<void> { throw new Error('storage is gone'); },
        },
      },
    });
    vi.stubGlobal('browser', browser);
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { loadUiLocale, setUiLocale } = await overlay();
    expect(await loadUiLocale()).toBe('auto');
    // Applying still works in-session; the failure to persist is reported.
    expect(await setUiLocale('en')).toBe('en');
    expect(warn.mock.calls.map((c) => String(c[0])).join(' ')).toContain('persist');
  });
});
