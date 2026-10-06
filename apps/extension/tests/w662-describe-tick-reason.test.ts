/**
 * W843 (W662) · **`describeTickReason` — the ten outcomes a tick can record, and
 * every code it cannot.**
 *
 * `describeTickReason` (lib/popup-view.ts) is the only place a scheduler outcome
 * code becomes a sentence a user reads: the popup's last-tick line and the
 * coverage view's tick row both call it, and neither call site can see whether the
 * code was the right one. Two properties have to hold, and only assertions here
 * hold them:
 *
 *   1. **Each code is worded by its own catalog entry.** The codes are kebab case
 *      on the record and camel case in `locales/*.yml`, so code → key is a
 *      hand-written mapping. Two of the ten exist precisely because they are easy
 *      to confuse — `host-paused` and `outbox-near-full` are both "this leg is
 *      waiting", and `no-targets` vs `no-http-port` were once conflated in the
 *      scheduler itself (C30). A mapping that swapped a pair would leave every
 *      other suite green while telling a reader to go and look in the wrong place.
 *   2. **A code this build cannot word is reported verbatim.** That is the first
 *      invariant of this project applied to a string: an outcome nobody has seen
 *      is a fact to be reported, not a gap to be blanked. Dropping it would put
 *      "nothing to say here" on a wake that did something, and a wrong sentence
 *      in its place would be worse — "a wrong reason is harder to investigate
 *      than no reason" (lib/backfill/schedule.ts).
 *
 * Every mapping assertion below compares against `t()` with the same key. That is
 * one equality written down twice, not a second oracle: what is under test is the
 * *code → key* pair, so the half of each assertion that carries information is the
 * code the case names. Whether those keys exist, are non-empty, and read alike in
 * both catalogs is tests/i18n-catalog.test.ts's business. The one thing that suite
 * cannot catch is added below: a sentence that comes out empty on both sides of
 * this file's equality.
 */

import { afterEach, describe, expect, it } from 'vitest';

import type { TickReason } from '../lib/backfill/schedule';
import { applyUiLocale, t } from '../lib/i18n';
import { describeTickReason } from '../lib/popup-view';

/**
 * 🔴 Every outcome the scheduler can write, and the catalog entry that words it.
 *
 * Typed `Record<TickReason, string>`, because the type is what makes this file
 * exhaustive rather than merely long. A member added to `TickReason` without a row
 * here fails `pnpm -s compile`, so the gap surfaces at the change that opened it —
 * instead of shipping and reading to whoever investigates that wake as its own name
 * in `outcome code: {reason}`, weeks later. A row for a code the scheduler cannot
 * produce is refused the same way: it would be a test asserting a mapping nothing
 * can reach.
 */
const WORDING: Record<TickReason, string> = {
  'no-targets': 'tick.reason.noTargets',
  'no-http-port': 'tick.reason.noHttpPort',
  'no-runnable-target': 'tick.reason.noRunnableTarget',
  'scope-asked': 'tick.reason.scopeAsked',
  disabled: 'tick.reason.disabled',
  'no-store': 'tick.reason.noStore',
  'host-paused': 'tick.reason.hostPaused',
  'outbox-near-full': 'tick.reason.outboxNearFull',
  'already-running': 'tick.reason.alreadyRunning',
  ran: 'tick.reason.ran',
};

/** The same pairs as a list. `describeTickReason` takes any `string`, so the element type stays wide; only `WORDING` is narrowed. */
const NAMED: ReadonlyArray<readonly [string, string]> = Object.entries(WORDING);

/**
 * `t()` resolves against module state in lib/i18n.ts, which is shared by every
 * case in this file, so a case that switches the language puts it back. Without
 * this the switch would leak into whichever case ran next.
 */
afterEach(async () => {
  await applyUiLocale('auto');
});

describe('W843 · describeTickReason', () => {
  describe('a code the scheduler can write', () => {
    it.each(NAMED)('%s is worded by its own catalog entry', (code, key) => {
      expect(describeTickReason(code)).toBe(t(key));
    });

    it('no two of them read alike, which is the whole point of naming them', () => {
      const sentences = NAMED.map(([code]) => describeTickReason(code));
      expect(new Set(sentences).size).toBe(NAMED.length);
    });

    it('each one is a sentence, never a blank line', () => {
      // The one thing this file's equality above cannot see: an empty catalog
      // entry would satisfy it on both sides at once and report success.
      for (const code of Object.keys(WORDING)) {
        expect(describeTickReason(code), `${code} is worded as nothing`).not.toBe('');
      }
    });

    it('all ten are worded in the language the popup is rendering in', async () => {
      // These sentences come from the catalog, not from source text: a string
      // pasted into popup-view.ts matches the English catalog byte for byte, so
      // every assertion above passes with one in place and nothing notices. Two
      // assertions per code, because either alone can pass for the wrong reason —
      // the equality says "the new locale's entry for this key", and the
      // difference says "and it really changed", which the equality would report
      // as a pass if a missing zh_CN entry silently fell back to English.
      const inEnglish = new Map(NAMED.map(([code]) => [code, describeTickReason(code)]));
      await applyUiLocale('zh_CN');
      for (const [code, key] of NAMED) {
        expect(describeTickReason(code), `${code} under zh_CN`).toBe(t(key));
        expect(describeTickReason(code), `${code} did not change language`)
          .not.toBe(inEnglish.get(code));
      }
    });
  });

  describe('a code no build can word', () => {
    it('is reported verbatim rather than swallowed', () => {
      expect(describeTickReason('some-new-reason'))
        .toBe(t('tick.reason.unknown', { reason: 'some-new-reason' }));
    });

    it.each(['no_targets', 'no-targets ', 'No-Targets', 'ran '])(
      // Exact match only. A near-miss is an unknown code, and saying so is the
      // honest answer: guessing which named outcome was meant is how a reader
      // ends up investigating the wrong thing.
      '%s is a near-miss, and is reported as itself',
      (code) => {
        expect(describeTickReason(code)).toBe(t('tick.reason.unknown', { reason: code }));
      },
    );

    it('the empty string is an unknown code, and still says something', () => {
      // 🔴 No reason recorded is not "no reason to show". The sentence carries the
      // empty code, so a reader sees that the wake happened and that this build
      // could not name its outcome.
      const said = describeTickReason('');
      expect(said).toBe(t('tick.reason.unknown', { reason: '' }));
      expect(said).not.toBe('');
    });
  });
});
