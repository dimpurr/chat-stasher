/**
 * 🔴 W821 · **Each mapping is pinned by the whole sentence it prints, not by a prefix of it.**
 *
 * The first version of this file compared the first eighteen characters of each catalog message.
 * For three of the eleven halt sentences those eighteen characters are the same string —
 * `orgUnresolved`, `workspaceAmbiguous` and `workspaceUnresolved` all begin "This leg stopped b" —
 * so one assertion was satisfied by three different sentences at once, and swapping any two of the
 * mappings in `lib/halt-note.ts` left the suite green. A prefix that several sentences share does
 * not name a mapping; it names a coincidence in the wording. Two sentences in this catalog open
 * that way deliberately (`authRefused` and `refusedUnknown`, which both begin by saying the stop is
 * not a platform change), so this is a property of the sentences rather than a mistake in one of
 * them, which is exactly why an assertion has to be built to survive it.
 *
 * Three things carry the mapping now, and each is load-bearing:
 *
 *   1. `sentence()` builds the expected string as the catalog's **own** message with `{name}`
 *      filled in, and every mapping is compared to it with `toBe`. Nothing shorter than the
 *      sentence satisfies that, so a swap fails loudly and the diff names both sentences.
 *   2. The last test refuses to let two halt sentences in one locale be the same text. Without it,
 *      the assertion above is only as strong as the catalog: collapse two messages into one
 *      wording and an exact comparison goes on passing while accepting a swapped mapping — the
 *      same hole, one layer down.
 *   3. Nothing here quotes a sentence. Every expected string is read out of the yml through
 *      `i18n-harness`, so this file cannot drift from what ships, and no Chinese has to be typed
 *      into a `.ts` file to assert something about the Chinese locale (`check-terminology.py`'s T5
 *      forbids it, and this file used to break it in eight places).
 *
 * The substitution in `sentence()` is written out here rather than borrowed from the product on
 * purpose. *Which* value belongs in *which* `{name}` hole is half of what `haltNote` decides, so a
 * helper that asked the product for the answer would be asking the thing under test to check
 * itself. A `{name}` the caller forgets is a refusal rather than a silent gap, because an expected
 * string with a literal `{platform}` left in it fails on a brace instead of on the mapping.
 */

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

function sentence(
  locale: TestLocale,
  key: string,
  values: Readonly<Record<string, string | number>> = {},
): string {
  return catalogText(locale, key).replace(/\{(\w+)\}/g, (_match, name: string) => {
    // 🔴 W821 · An expectation is allowed to name a value but not to omit one. A `{name}`
    //    left unfilled would leave the literal brace in the expected string, and the failure
    //    would then be about punctuation instead of about the mapping it was written for.
    if (!Object.hasOwn(values, name)) {
      throw new Error(`${key} interpolates {${name}}, and this expectation supplies no value for it`);
    }
    return String(values[name]);
  });
}

/**
 * Every sentence the halt-note family can print, read out of the catalog rather than listed here.
 * A new halt reason adds its message to the yml and lands in this set for free, so the
 * distinctness check below cannot quietly go on covering less than the file does.
 */
function haltSentences(locale: TestLocale): Map<string, string> {
  return new Map(
    Object.entries(CATALOGS[locale])
      .filter(([key]) => key.startsWith('popup_notes_halted_'))
      .map(([key, entry]) => [key, entry.message]),
  );
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
    // The exact sentence carries the platform name and the empty detail at once: the label is
    // still there, with nothing after it, which is what "a halt record always has a detail slot"
    // has to mean for a record whose detail is an empty string rather than a missing one.
    expect(note).toBe(sentence(locale, 'popup.notes.halted.unsupportedPlatform', {
      platform: 'synthetic-platform',
      detail: '',
    }));
    expect(note).not.toContain('synthetic detail');
    expect(note).not.toContain('undefined');
  });

  it('maps auth-refused with its own message, interpolation, and rounded retry time', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('auth-refused', 'synthetic auth detail', {
      attempts: 3,
      retryAt: NOW + 60_001,
    }), 'synthetic-platform', 0, NOW)!;
    expect(note).toBe(sentence(locale, 'popup.notes.halted.authRefused', {
      platform: 'synthetic-platform',
      detail: 'synthetic auth detail',
      attempts: 3,
      minutes: 2,
    }));
  });

  it('maps refused-unknown separately from auth-refused', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('refused-unknown', 'synthetic code 91', {
      attempts: 2,
      retryAt: NOW + 1,
    }), 'synthetic-platform', 0, NOW)!;
    expect(note).toBe(sentence(locale, 'popup.notes.halted.refusedUnknown', {
      platform: 'synthetic-platform',
      detail: 'synthetic code 91',
      attempts: 2,
      minutes: 1,
    }));
    // 🔴 W821 · This arm exists *because* an unreadable code is not a login problem, so the
    //    assertion is between the two refusals rather than about one of them. Given the same
    //    platform, detail and numbers, the refusal that names a readable login and the one that
    //    names a code this build cannot read are two different sentences — which says it in both
    //    locales without either sentence's words appearing in this file.
    const refusedLogin = haltNote(halt('auth-refused', 'synthetic auth detail', {
      attempts: 3,
      retryAt: NOW + 60_001,
    }), 'synthetic-platform', 0, NOW)!;
    expect(note).not.toBe(refusedLogin);
  });

  it('maps other transient reasons to the waiting sentence and pins retry-minute boundaries', async () => {
    const haltNote = await useLocale(locale);
    const reason = 'transport-error';
    const detail = 'synthetic transport detail';
    const render = (retryAt?: number) => haltNote(halt(reason, detail, {
      attempts: 4,
      ...(retryAt === undefined ? {} : { retryAt }),
    }), 'synthetic-platform', 0, NOW)!;
    const waiting = (minutes: number) => sentence(locale, 'popup.notes.halted.waitingRetry', {
      reason,
      attempts: 4,
      minutes,
      detail,
    });

    // 🔴 W821 · The rounding is read off the whole sentence now rather than off a fragment of
    //    it. `retryMinutesLeft` is the thing under test here, and the number it returns is only
    //    visible in the sentence that prints it — a phrase built from a literal can drift from the
    //    wording around it and still go green, which is how the two minutes here used to be
    //    asserted without either the sentence or the ceiling being pinned.
    expect(render()).toBe(waiting(0));
    expect(render(NOW - 1)).toBe(waiting(0));
    expect(render(NOW)).toBe(waiting(0));
    expect(render(NOW + 1)).toBe(waiting(1));
    expect(render(NOW + 60_000)).toBe(waiting(1));
    expect(render(NOW + 60_001)).toBe(waiting(2));
  });

  it.each([
    ['org-ambiguous', 'chatgpt', 'workspaceAmbiguous'],
    ['org-unresolved', 'chatgpt', 'workspaceUnresolved'],
    ['org-ambiguous', 'claude', 'orgAmbiguous'],
    ['org-unresolved', 'claude', 'orgUnresolved'],
  ])('maps %s on %s to the matching organization sentence', async (reason, platform, key) => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt(reason, 'synthetic resolver detail'), platform, 0, NOW)!;
    // 🔴 W821 · **The four sentences this table pins, and the hole this replaced.** Three of
    //    them open with the same eighteen characters, so the first version of this case compared
    //    that prefix and could not tell `workspaceAmbiguous` from `workspaceUnresolved` from
    //    `orgUnresolved` — swapping any two of the mappings in `lib/halt-note.ts` left it green.
    //    The whole sentence is the assertion now, which is why these four need no placeholder
    //    values: they carry none, on purpose, so the detail stays on the record and the action is
    //    what the user is shown. `sentence()` refusing an unfilled `{name}` is that fact enforced.
    expect(note).toBe(sentence(locale, `popup.notes.halted.${key}`));
    expect(note).not.toContain('synthetic resolver detail');
  });

  it('maps account-changed without describing it as an automatic retry', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('account-changed', 'synthetic account detail'), 'claude', 0, NOW)!;
    expect(note).toBe(sentence(locale, 'popup.notes.halted.accountChanged', {
      detail: 'synthetic account detail',
    }));
    // 🔴 W821 · **The ordering, stated as a difference between two rendered sentences.**
    //    `account-changed` classifies as transient, so this is the case the branch's reordering
    //    exists for: if the generic transient arm is reached first, the user is told the leg will
    //    come back by itself, which for this stop is exactly the false thing — the same request
    //    under the same wrong account gets it nowhere. Comparing against what the transient arm
    //    prints for the same record makes the ordering observable in both locales.
    const waiting = haltNote(halt('transport-error', 'transport detail', { attempts: 4 }), 'claude', 0, NOW)!;
    expect(note).not.toBe(waiting);
  });

  it('maps detail-unsupported and interpolates the pending count, including zero', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('detail-unsupported', 'synthetic detail capability'), 'synthetic-platform', 0, NOW)!;
    expect(note).toBe(sentence(locale, 'popup.notes.halted.detailUnsupported', {
      platform: 'synthetic-platform',
      pending: 0,
      detail: 'synthetic detail capability',
    }));
  });

  it('uses the generic fallback for an unrecognized reason', async () => {
    const haltNote = await useLocale(locale);
    const note = haltNote(halt('future-reason', 'synthetic future detail'), 'claude', 0, NOW)!;
    expect(note).toBe(sentence(locale, 'popup.notes.halted.other', {
      reason: 'future-reason',
      detail: 'synthetic future detail',
    }));
  });

  it('gives every halt sentence in this locale a text of its own', () => {
    // 🔴 W821 · The property the exact comparisons above rest on, and the one that made the
    //    prefix comparison wrong in the first place. If two of these sentences ever share their
    //    text then the two mappings become interchangeable however exactly each is compared, so
    //    this refuses that here — naming both keys — rather than leaving the next reader to spot
    //    it. Read out of the catalog, so a newly added halt message is checked without an edit.
    const seen = new Map<string, string>();
    for (const [key, message] of haltSentences(locale)) {
      const twin = seen.get(message);
      expect(twin, `${key} and ${twin ?? 'no other key'} print the same sentence`).toBeUndefined();
      seen.set(message, key);
    }
    expect(seen.size).toBeGreaterThan(1);
  });
});
