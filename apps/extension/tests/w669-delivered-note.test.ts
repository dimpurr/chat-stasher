/**
 * W669 · The three `ui-strings` builders that had no direct test of their own.
 *
 * `deliveredNote`, `exportEmptyNote` and `exportNothingQueued` were covered only
 * insofar as some render path happened to call them, which is not the same as
 * pinning them: a builder may swap its key, drop a placeholder, or invent a
 * default and still produce a plausible-looking sentence somewhere else in the
 * popup. Each case below therefore asserts the builder against the *same* `t`
 * the shipped code calls, with the *same* arguments — so a drift in key,
 * placeholder set or timestamp formatting is a red run rather than a
 * differently-worded screen.
 *
 * The expected side goes through tests/i18n-harness.ts, which compiles the real
 * `locales/en.yml` with the package's own compiler: the wording asserted here is
 * the wording that ships. This file owns the wiring, never the copy.
 *
 * `deliveredNote` gets three counts on purpose — `0` is the boundary a plural
 * entry would switch on, `1` the singular, `42` an ordinary plural — and none of
 * them may lose `{at}`. The delivered record is written once and never cleared,
 * so the time is the whole reason it stores one (lib/ui-strings.ts, W214b review
 * finding 2).
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { CATALOGS, withI18n } from './i18n-harness';

/** A browser whose `i18n` answers English from the shipped catalog. */
const fakeBrowser = withI18n({ runtime: { id: 'w669-delivered-note-test' } }, 'en');

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
});

afterEach(() => {
  vi.restoreAllMocks();
});

/**
 * The overlay's `t` and the builders under test, imported fresh so every case
 * runs on the module instance the stub above is the browser for — `lib/i18n.ts`
 * captures its catalog (and `@wxt-dev/browser` its API object) at import time.
 */
async function uiStrings() {
  const { t } = await import('../lib/i18n');
  const ui = await import('../lib/ui-strings');
  return { t, ui };
}

const AT_EPOCH_ZERO = 0;
const AT_ONE = 1_700_000_000_000;

describe('W669 · deliveredNote states the count and the moment it happened', () => {
  it('zero counts are rendered through the catalog entry, not short-circuited', async () => {
    const { t, ui } = await uiStrings();
    expect(ui.deliveredNote(0, AT_EPOCH_ZERO))
      .toBe(t('extensionOnly.delivered', { count: 0, at: ui.stamp(AT_EPOCH_ZERO) }));
  });

  it('a single delivery reads through the same key', async () => {
    const { t, ui } = await uiStrings();
    expect(ui.deliveredNote(1, AT_EPOCH_ZERO))
      .toBe(t('extensionOnly.delivered', { count: 1, at: ui.stamp(AT_EPOCH_ZERO) }));
  });

  it('a plural count reads through the same key', async () => {
    const { t, ui } = await uiStrings();
    expect(ui.deliveredNote(42, AT_ONE))
      .toBe(t('extensionOnly.delivered', { count: 42, at: ui.stamp(AT_ONE) }));
  });

  it('🔴 the sentence carries both facts — the count and the moment — with no placeholder left', async () => {
    const { ui } = await uiStrings();
    const note = ui.deliveredNote(42, AT_ONE);
    expect(note).toContain('42');
    expect(note).toContain(ui.stamp(AT_ONE));
    expect(note).not.toContain('{');
    // Two drains with the same count at different times must not be the same
    // sentence: without `{at}` they would be, which is the defect W214b found.
    expect(ui.deliveredNote(42, AT_ONE - 90 * 24 * 60 * 60 * 1000)).not.toBe(note);
  });

  it('the timestamp is formatted by the shared stamp, in the same form the popup shows elsewhere', async () => {
    const { ui } = await uiStrings();
    expect(ui.stamp(AT_EPOCH_ZERO)).toBe('1970-01-01 00:00:00 UTC');
    expect(ui.deliveredNote(0, AT_EPOCH_ZERO)).toContain('1970-01-01 00:00:00 UTC');
  });
});

describe('W669 · the two export notes are the catalog entries they claim to be', () => {
  it('exportEmptyNote is export.emptyNote', async () => {
    const { t, ui } = await uiStrings();
    expect(ui.exportEmptyNote()).toBe(t('export.emptyNote'));
    expect(ui.exportEmptyNote()).toBe(CATALOGS.en.export_emptyNote!.message);
  });

  it('exportNothingQueued is export.nothingQueued', async () => {
    const { t, ui } = await uiStrings();
    expect(ui.exportNothingQueued()).toBe(t('export.nothingQueued'));
    expect(ui.exportNothingQueued()).toBe(CATALOGS.en.export_nothingQueued!.message);
  });

  it('the two are two different situations, and say different things', async () => {
    const { ui } = await uiStrings();
    // An empty outbox and a press that queued nothing are not the same
    // observation; one entry rendering both would erase the difference.
    expect(ui.exportEmptyNote()).not.toBe(ui.exportNothingQueued());
    expect(ui.exportEmptyNote()).toContain('outbox is empty');
    expect(ui.exportNothingQueued()).toBe('Nothing to export.');
  });
});
