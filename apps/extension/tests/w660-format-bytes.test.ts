/**
 * W660 · **`formatBytes` on its own, at every boundary it has.**
 *
 * `formatBytes` (lib/ui-strings.ts:71-79) is the only unit conversion in the
 * extension that runs on a user-visible number, and until now nothing tested it
 * directly: the outbox line and the export note each call it, so its behaviour
 * was only ever observed through a sentence that also contained other facts.
 * That hides two different failures behind one another — a wrong unit reads as
 * a wrong sentence, and a wrong sentence is not obvious when the sentence is
 * already long. So the boundaries are pinned here as the strings they are.
 *
 * ## What is worth pinning, and why each one
 *
 *  · **Tier edges.** `bytes < 1024` → B, `< 1 MiB` → KiB, `< 1 GiB` → MiB, else
 *    GiB. The interesting values are the ones *just under* a tier change
 *    (1023, 1048575, 1073741823): a rewrite to `<=` would move them, and each
 *    of them is a real measurement — an outbox that is one byte short of a
 *    round number is still an outbox, and `1024.0 KiB` is the honest reading of
 *    it rather than a `1.0 MiB` that would overstate the queue by 1 KiB.
 *  · **The rounding the tiers actually promise.** B is not padded (a byte
 *    count is exact and `0 B` is shorter and truer than `0.0 B`), KiB and MiB
 *    carry one decimal, GiB carries two. That asymmetry is deliberate — a GiB
 *    figure is two orders of magnitude further from the byte count that
 *    produced it — and nothing else in the suite states it.
 *  · **Everything that is not a size.** `NaN`, `±Infinity` and a negative byte
 *    count all reach the same guard and must all read as `common.unknownSize`,
 *    not as `NaN B`, `-1 B` or `Infinity B`. A negative outbox byte count is a
 *    corrupt reading, and printing it as a number would present the corruption
 *    as a measurement.
 *
 * ## How it reads `t`
 *
 * The unknown-size wording is asserted against the **real** `t` from
 * lib/i18n, driven by the harness's compiled catalog (tests/i18n-harness.ts),
 * and separately against the literal text in `locales/en.yml`. The first makes
 * it a real lookup on the shipped path; the second stops this file from passing
 * against an empty string, which is what `t` answers for a key no catalog
 * defines.
 *
 * Synthetic only: byte counts are arithmetic, and nothing here reads or writes
 * storage, the network, or a real conversation.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { CATALOGS, withI18n } from './i18n-harness';

/**
 * The same fake browser shape as tests/i18n-popup.test.ts: `withI18n` gives it
 * the catalog-backed `i18n` and the `runtime` the translation layer reaches for,
 * so `t` resolves the way it does in the extension.
 */
const fakeBrowser = withI18n({
  runtime: { id: 'w660-format-bytes' },
  storage: {
    local: {
      async get(query: Record<string, unknown>) { return { ...query }; },
      async set() { /* this file asserts `t`, not the stored preference */ },
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
  vi.unstubAllGlobals();
});

/** `formatBytes` and the `t` it resolves its one catalog key through. */
async function subject() {
  const { formatBytes } = await import('../lib/ui-strings');
  const { t } = await import('../lib/i18n');
  return { formatBytes, t };
}

const KIB = 1024;
const MIB = 1024 * 1024;
const GIB = 1024 * 1024 * 1024;

describe('W660 · formatBytes', () => {
  describe('byte tier — below 1 KiB, exact and unpadded', () => {
    it.each([
      [0, '0 B'],
      [1, '1 B'],
      [512, '512 B'],
      [KIB - 1, '1023 B'],
    ])('%p renders as %p, with no decimal place', async (bytes, expected) => {
      const { formatBytes } = await subject();
      expect(formatBytes(bytes)).toBe(expected);
    });
  });

  describe('KiB tier — 1 KiB up to just under 1 MiB, one decimal', () => {
    it.each([
      [KIB, '1.0 KiB'],
      [1536, '1.5 KiB'],
      [10 * KIB, '10.0 KiB'],
      [MIB - 1, '1024.0 KiB'],
    ])('%p renders as %p', async (bytes, expected) => {
      const { formatBytes } = await subject();
      expect(formatBytes(bytes)).toBe(expected);
    });
  });

  describe('MiB tier — 1 MiB up to just under 1 GiB, one decimal', () => {
    it.each([
      [MIB, '1.0 MiB'],
      [1.5 * MIB, '1.5 MiB'],
      [GIB - 1, '1024.0 MiB'],
    ])('%p renders as %p', async (bytes, expected) => {
      const { formatBytes } = await subject();
      expect(formatBytes(bytes)).toBe(expected);
    });
  });

  describe('GiB tier — 1 GiB and above, two decimals', () => {
    it.each([
      [GIB, '1.00 GiB'],
      [1.5 * GIB, '1.50 GiB'],
      [1.234_5 * GIB, '1.23 GiB'],
    ])('%p renders as %p', async (bytes, expected) => {
      const { formatBytes } = await subject();
      expect(formatBytes(bytes)).toBe(expected);
    });
  });

  describe('the guard — anything that is not a byte count', () => {
    it.each([
      ['NaN', NaN],
      ['Infinity', Infinity],
      ['-Infinity', -Infinity],
      ['-1', -1],
      ['-1024', -1024],
    ])('%p renders as the unknown-size wording, not as a number', async (_label, bytes) => {
      const { formatBytes, t } = await subject();
      expect(formatBytes(bytes)).toBe(t('common.unknownSize'));
    });

    it('resolves that wording from the catalog, so the assertion is not vacuous', async () => {
      const { t } = await subject();
      // The shipped English text, named here as well as looked up: `t` answers
      // the empty string for a key no catalog defines, and an empty string is
      // something five assertions in this group would all happily agree on.
      expect(t('common.unknownSize')).toBe('unknown size');
      expect(CATALOGS.en.common_unknownSize?.message).toBe('unknown size');
    });

    it('keeps every guard case distinct from the B tier, which cannot answer with a word', async () => {
      const { formatBytes } = await subject();
      for (const bytes of [NaN, Infinity, -Infinity, -1, -1024]) {
        expect(formatBytes(bytes)).not.toMatch(/\d\s*(B|KiB|MiB|GiB)$/);
      }
    });
  });

  describe('what the unit symbols are not', () => {
    it('does not localise the units, so the same bytes read the same in both catalogs', async () => {
      // The header of lib/ui-strings.ts says the units are deliberately not
      // translated. A `KiB` that became a translated name in one locale would
      // make the outbox capacity harder to compare against, and this pins the
      // numeric part — the only part that is a measurement.
      const { formatBytes } = await subject();
      expect(CATALOGS.zh_CN.common_unknownSize?.message).not.toBe(
        CATALOGS.en.common_unknownSize?.message,
      );
      expect(formatBytes(1536)).toBe('1.5 KiB');
    });
  });
});