/**
 * The two catalog files must stay in step.
 *
 * A catalog pair drifts in exactly one direction: someone adds an entry to the
 * default locale, ships it, and the other locale silently renders the default
 * language for that one label forever. Nobody notices, because nothing breaks —
 * the page is half translated and every test is green. That is why these are
 * assertions and not a review checklist.
 *
 * The alignment is checked through `@wxt-dev/i18n/build`'s own parser, which is
 * the same one the build uses, so this test cannot disagree with what ships.
 */

import { describe, it, expect } from 'vitest';
import { fileURLToPath } from 'node:url';
import { parseMessagesFile, type ParsedMessage } from '@wxt-dev/i18n/build';
import { CATALOGS, TEST_LOCALES, type TestLocale } from './i18n-harness';

function file(locale: TestLocale): string {
  return fileURLToPath(new URL(`../locales/${locale}.yml`, import.meta.url));
}

/** Keys, minus the `@@` predefined messages the parser injects for us. */
function keysOf(messages: ParsedMessage[]): string[] {
  return messages
    .map((m) => m.key.join('.'))
    .filter((k) => !k.startsWith('@@'))
    .sort();
}

function byKey(messages: ParsedMessage[]): Map<string, ParsedMessage> {
  return new Map(messages.map((m) => [m.key.join('.'), m]));
}

describe('i18n catalog · en.yml and zh_CN.yml stay aligned', () => {
  it('both catalogs parse, and hold exactly the same key set', async () => {
    const en = await parseMessagesFile(file('en'));
    const zh = await parseMessagesFile(file('zh_CN'));
    const enKeys = keysOf(en);
    expect(enKeys.length).toBeGreaterThan(50);
    // toEqual on the sorted arrays names the offending key when one is added to
    // only one side, which a length check would not.
    expect(keysOf(zh)).toEqual(enKeys);
  });

  it('every entry carries the same placeholders on both sides', async () => {
    const en = byKey(await parseMessagesFile(file('en')));
    const zh = byKey(await parseMessagesFile(file('zh_CN')));
    for (const [key, a] of en) {
      const b = zh.get(key)!;
      expect(b, `zh_CN is missing ${key}`).toBeDefined();
      expect(b.namedSubstitutions, `placeholders differ for ${key}`)
        .toEqual(a.namedSubstitutions);
      expect(b.substitutions, `positional substitution count differs for ${key}`)
        .toBe(a.substitutions);
      // Only simple and plural entries carry `plural`; a verbose ("chrome")
      // entry has no such field, and absent means "not a plural entry".
      const pluralOf = (m: ParsedMessage) => ('plural' in m ? m.plural : false);
      expect(pluralOf(b), `plural flag differs for ${key}`).toBe(pluralOf(a));
    }
  });

  it('no entry uses positional $1-style placeholders', async () => {
    // The catalog deliberately uses {name} only: that is the one form the
    // package's own path and the overlay's path reproduce identically (see the
    // header of lib/i18n.ts). Positional substitution is still implemented and
    // tested there — this asserts nobody quietly starts using it, which would
    // make the two paths agree only by luck.
    for (const locale of TEST_LOCALES) {
      const messages = await parseMessagesFile(file(locale));
      for (const m of messages) {
        if (m.key[0]!.startsWith('@@')) continue;
        expect(m.substitutions, `${locale}: ${m.key.join('.')} uses positional placeholders`)
          .toBe(0);
      }
    }
  });

  it('the language selector labels read the same in both catalogs', () => {
    // Each option is written in its own script — the language's endonym — so a
    // user who switched language by mistake can still find their own in the list.
    // That only works if the two catalogs agree on them, character for character.
    const en = CATALOGS.en;
    const zh = CATALOGS.zh_CN;
    for (const key of ['popup_locale_auto', 'popup_locale_english', 'popup_locale_chinese']) {
      expect(en[key]?.message, `en is missing ${key}`).toBeTruthy();
      expect(zh[key]?.message, `zh_CN is missing ${key}`).toBeTruthy();
      expect(zh[key]!.message).toBe(en[key]!.message);
    }
  });

  it('every entry has non-empty text in both catalogs', () => {
    for (const locale of TEST_LOCALES) {
      for (const [key, entry] of Object.entries(CATALOGS[locale])) {
        if (key.startsWith('@@')) continue;
        expect(entry.message.trim().length, `${locale}: ${key} is empty`).toBeGreaterThan(0);
      }
    }
  });
});
