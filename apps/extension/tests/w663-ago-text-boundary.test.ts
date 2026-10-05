import { describe, it, expect } from 'vitest';

import { t } from '../lib/i18n';
import { agoText } from '../lib/popup-view';
import { CATALOGS } from './i18n-harness';

function messageOf(dottedKey: string): string {
  const message = CATALOGS.en[dottedKey.replaceAll('.', '_')]?.message;
  if (typeof message !== 'string') throw new Error(`en catalog is missing ${dottedKey}`);
  return message;
}

function rendered(dottedKey: string, count: number): string {
  return messageOf(dottedKey).replaceAll('{count}', String(count));
}

describe('W663 · agoText boundary set', () => {
  describe('the just-now tier · 0 to 89 seconds', () => {
    it('0 seconds is just now', () => {
      expect(agoText(0)).toBe(t('popup.summary.agoJustNow'));
      expect(agoText(0)).toBe(messageOf('popup.summary.agoJustNow'));
    });

    it('1 second is just now', () => {
      expect(agoText(1)).toBe(t('popup.summary.agoJustNow'));
      expect(agoText(1)).toBe(messageOf('popup.summary.agoJustNow'));
    });

    it('89 seconds is just now — the last input before the minutes tier', () => {
      expect(agoText(89)).toBe(t('popup.summary.agoJustNow'));
      expect(agoText(89)).toBe(messageOf('popup.summary.agoJustNow'));
    });
  });

  describe('the minutes tier · 90 seconds to 5369 seconds', () => {
    it('90 seconds rounds up to 2 minutes — 1 minute is unreachable because the just-now tier takes 60 to 89 seconds', () => {
      expect(agoText(90)).toBe(t('popup.summary.agoMinutes', { count: 2 }));
      expect(agoText(90)).toBe(rendered('popup.summary.agoMinutes', 2));
    });

    it('91 seconds is still 2 minutes', () => {
      expect(agoText(91)).toBe(t('popup.summary.agoMinutes', { count: 2 }));
      expect(agoText(91)).toBe(rendered('popup.summary.agoMinutes', 2));
    });

    it('5369 seconds is 89 minutes — the last input the minutes tier holds', () => {
      expect(agoText(5369)).toBe(t('popup.summary.agoMinutes', { count: 89 }));
      expect(agoText(5369)).toBe(rendered('popup.summary.agoMinutes', 89));
    });

    it('5399 seconds is just under 90 minutes, but rounds to 90, which is not < 90, so it falls through to the hours tier', () => {
      expect(agoText(5399)).toBe(t('popup.summary.agoHours', { count: 2 }));
      expect(agoText(5399)).toBe(rendered('popup.summary.agoHours', 2));
    });
  });

  describe('the hours tier · 5370 seconds to 170969 seconds', () => {
    it('5370 seconds is 89.5 minutes, which rounds to 90 minutes = 1.5 hours, which rounds to 2 hours — the first input the hours tier holds', () => {
      expect(agoText(5370)).toBe(t('popup.summary.agoHours', { count: 2 }));
      expect(agoText(5370)).toBe(rendered('popup.summary.agoHours', 2));
    });

    it('5400 seconds is 90 minutes = 1.5 hours, which rounds to 2 hours', () => {
      expect(agoText(5400)).toBe(t('popup.summary.agoHours', { count: 2 }));
      expect(agoText(5400)).toBe(rendered('popup.summary.agoHours', 2));
    });

    it('86399 seconds is one second short of a full 24 hours, and rounds to 24 hours', () => {
      expect(agoText(86399)).toBe(t('popup.summary.agoHours', { count: 24 }));
      expect(agoText(86399)).toBe(rendered('popup.summary.agoHours', 24));
    });

    it('86400 seconds is exactly 24 hours, and 24 < 48, so it is still hours', () => {
      expect(agoText(86400)).toBe(t('popup.summary.agoHours', { count: 24 }));
      expect(agoText(86400)).toBe(rendered('popup.summary.agoHours', 24));
    });

    it('170969 seconds is 2849 minutes, which rounds to 47 hours — the last input the hours tier holds, because 2850 minutes rounds to 48, and 48 is not < 48', () => {
      expect(agoText(170969)).toBe(t('popup.summary.agoHours', { count: 47 }));
      expect(agoText(170969)).toBe(rendered('popup.summary.agoHours', 47));
    });
  });

  describe('the days tier · 170970 seconds and beyond', () => {
    it('170970 seconds rounds to 2850 minutes, which is 47.5 hours, which rounds to 48, and 48 is not < 48, so it falls through to days: 2', () => {
      expect(agoText(170970)).toBe(t('popup.summary.agoDays', { count: 2 }));
      expect(agoText(170970)).toBe(rendered('popup.summary.agoDays', 2));
    });

    it('172800 seconds is 48 hours, which is not < 48, so it falls through to days: 2', () => {
      expect(agoText(172800)).toBe(t('popup.summary.agoDays', { count: 2 }));
      expect(agoText(172800)).toBe(rendered('popup.summary.agoDays', 2));
    });
  });

  describe('the unknown tier · not a finite non-negative number', () => {
    it('NaN is unknown', () => {
      expect(agoText(NaN)).toBe(t('common.unknownTimeShort'));
      expect(agoText(NaN)).toBe(messageOf('common.unknownTimeShort'));
    });

    it('Infinity is unknown', () => {
      expect(agoText(Infinity)).toBe(t('common.unknownTimeShort'));
      expect(agoText(Infinity)).toBe(messageOf('common.unknownTimeShort'));
    });

    it('-1 is unknown', () => {
      expect(agoText(-1)).toBe(t('common.unknownTimeShort'));
      expect(agoText(-1)).toBe(messageOf('common.unknownTimeShort'));
    });

    it('-86400 is unknown', () => {
      expect(agoText(-86400)).toBe(t('common.unknownTimeShort'));
      expect(agoText(-86400)).toBe(messageOf('common.unknownTimeShort'));
    });
  });
});
