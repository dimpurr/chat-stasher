/**
 * The tier boundaries of `agoText` (lib/popup-view.ts), which turns an age in
 * seconds into one of four renderings. The rounding is nested and lossy at every
 * step, so each tier is pinned at the input it stops being able to hold — the
 * last one it keeps and the first one the next tier takes:
 *
 *   just now  0-89          (seconds < 90)
 *   minutes   90-5369       (round(seconds/60) < 90)
 *   hours     5370-170969   (round(minutes/60) < 48)
 *   days      170970+       (round(hours/24))
 *   unknown   not finite, or negative
 *
 * Two of those edges are not where the arithmetic makes them look, which is the
 * whole reason they are pinned here:
 *
 *   • 5370 s is where the minutes tier lets go. round(5369/60) = 89, which
 *     renders; round(5370/60) = 90, and 90 is not < 90, so it falls through.
 *     60 to 89 seconds are gone entirely — they round to 1 minute but are claimed
 *     by "just now" first — so "1 min ago" is not reachable at all.
 *   • The hours tier does not end at a day, and it does not end at 48 hours
 *     either. It ends where round(minutes/60) first reaches 48, which is 2850
 *     minutes = 170970 s. So its last input is 170969 s, which renders as 47 h,
 *     and the next one renders as 2 d — while 86399 s and 86400 s both render as
 *     24 h, a full day inside the tier.
 *
 * One assertion per case, and it is the `t()` one. In this suite `t` reaches the
 * compiled `locales/en.yml` through the fake `chrome.i18n` that tests/setup.ts
 * installs (tests/i18n-harness.ts), so an expected string read out of that same
 * catalog is not a second oracle — it is this one equality written down twice,
 * and a pair like that reads as corroboration while adding nothing. That the
 * keys used here exist, are non-empty, and are aligned across both catalogs is
 * tests/i18n-catalog.test.ts's business.
 */

import { describe, it, expect } from 'vitest';

import { t } from '../lib/i18n';
import { agoText } from '../lib/popup-view';

describe('W663 · agoText boundary set', () => {
  describe('the just-now tier · 0 to 89 seconds', () => {
    it('0 seconds is just now', () => {
      expect(agoText(0)).toBe(t('popup.summary.agoJustNow'));
    });

    it('1 second is just now', () => {
      expect(agoText(1)).toBe(t('popup.summary.agoJustNow'));
    });

    it('89 seconds is just now — the last input before the minutes tier', () => {
      expect(agoText(89)).toBe(t('popup.summary.agoJustNow'));
    });
  });

  describe('the minutes tier · 90 seconds to 5369 seconds', () => {
    it('90 seconds rounds up to 2 minutes — 1 minute is unreachable, because 60 to 89 seconds are claimed by the just-now tier first', () => {
      expect(agoText(90)).toBe(t('popup.summary.agoMinutes', { count: 2 }));
    });

    it('91 seconds is still 2 minutes', () => {
      expect(agoText(91)).toBe(t('popup.summary.agoMinutes', { count: 2 }));
    });

    it('5369 seconds is 89 minutes — the last input the minutes tier holds', () => {
      expect(agoText(5369)).toBe(t('popup.summary.agoMinutes', { count: 89 }));
    });
  });

  describe('the hours tier · 5370 seconds to 170969 seconds', () => {
    it('5370 seconds is 89.5 minutes, which rounds to 90 minutes, which is not < 90, so it drops to 1.5 hours, which rounds to 2 — the first input the hours tier holds', () => {
      expect(agoText(5370)).toBe(t('popup.summary.agoHours', { count: 2 }));
    });

    it('5399 seconds is 89 minutes 59 seconds, which rounds to 90, which is not < 90, so it never renders as minutes either', () => {
      expect(agoText(5399)).toBe(t('popup.summary.agoHours', { count: 2 }));
    });

    it('5400 seconds is 90 minutes = 1.5 hours, which rounds to 2 hours', () => {
      expect(agoText(5400)).toBe(t('popup.summary.agoHours', { count: 2 }));
    });

    it('86399 seconds is one second short of a full day, and still renders as 24 hours', () => {
      expect(agoText(86399)).toBe(t('popup.summary.agoHours', { count: 24 }));
    });

    it('86400 seconds is exactly 24 hours, and 24 < 48, so it is still hours', () => {
      expect(agoText(86400)).toBe(t('popup.summary.agoHours', { count: 24 }));
    });

    it('170969 seconds is 2849 minutes, which rounds to 47 hours — the last input the hours tier holds, because 2850 minutes rounds to 48, and 48 is not < 48', () => {
      expect(agoText(170969)).toBe(t('popup.summary.agoHours', { count: 47 }));
    });
  });

  describe('the days tier · 170970 seconds and beyond', () => {
    it('170970 seconds rounds to 2850 minutes, which is 47.5 hours, which rounds to 48, and 48 is not < 48, so it falls through to days: 2', () => {
      expect(agoText(170970)).toBe(t('popup.summary.agoDays', { count: 2 }));
    });

    it('172800 seconds is 48 hours, which is not < 48, so it falls through to days: 2', () => {
      expect(agoText(172800)).toBe(t('popup.summary.agoDays', { count: 2 }));
    });
  });

  describe('the unknown tier · not a finite non-negative number', () => {
    it('NaN is unknown', () => {
      expect(agoText(NaN)).toBe(t('common.unknownTimeShort'));
    });

    it('Infinity is unknown', () => {
      expect(agoText(Infinity)).toBe(t('common.unknownTimeShort'));
    });

    it('-1 is unknown', () => {
      expect(agoText(-1)).toBe(t('common.unknownTimeShort'));
    });

    it('-86400 is unknown', () => {
      expect(agoText(-86400)).toBe(t('common.unknownTimeShort'));
    });
  });
});