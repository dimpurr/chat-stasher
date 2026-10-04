import { describe, expect, it } from 'vitest';
import { HOOK_REASON_WAS_REPLACED } from '../lib/contract';
import {
  HOOK_STATUS_STALE_AFTER_MS,
  isHookStatusStale,
  looksLikeHookStatus,
  type HookStatusRecord,
} from '../lib/hook-status';

const VALID_RECORD: HookStatusRecord = {
  origin: 'https://synthetic.example',
  platform: 'chatgpt',
  reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at: 10_000, since: 9_000 }],
  at: 10_000,
};

describe('hook status time boundaries', () => {
  it.each([
    ['record timestamp NaN', { ...VALID_RECORD, at: Number.NaN }],
    ['record timestamp Infinity', { ...VALID_RECORD, at: Number.POSITIVE_INFINITY }],
    [
      'observation timestamp NaN',
      { ...VALID_RECORD, reasons: [{ ...VALID_RECORD.reasons[0], at: Number.NaN }] },
    ],
    [
      'observation timestamp Infinity',
      { ...VALID_RECORD, reasons: [{ ...VALID_RECORD.reasons[0], at: Number.POSITIVE_INFINITY }] },
    ],
  ])('rejects a non-finite %s', (_label, value) => {
    expect(looksLikeHookStatus(value)).toBe(false);
  });

  it.each([
    ['NaN', Number.NaN],
    ['Infinity', Number.POSITIVE_INFINITY],
    ['a string', '9_000'],
    ['null', null],
  ])('rejects malformed since value: %s', (_label, since) => {
    const value = {
      ...VALID_RECORD,
      reasons: [{ ...VALID_RECORD.reasons[0]!, since }],
    };

    expect(looksLikeHookStatus(value)).toBe(false);
  });

  it('accepts a missing since because it was not recorded', () => {
    const { since: _since, ...legacyObservation } = VALID_RECORD.reasons[0]!;
    expect(looksLikeHookStatus({
      ...VALID_RECORD,
      reasons: [legacyObservation],
    })).toBe(true);
  });

  it('considers the exact stale threshold current and one millisecond beyond stale', () => {
    const now = 1_700_000_000_000;
    const atThreshold = {
      ...VALID_RECORD,
      at: now - HOOK_STATUS_STALE_AFTER_MS,
    };
    const beyondThreshold = {
      ...VALID_RECORD,
      at: now - HOOK_STATUS_STALE_AFTER_MS - 1,
    };

    expect(isHookStatusStale(atThreshold, now)).toBe(false);
    expect(isHookStatusStale(beyondThreshold, now)).toBe(true);
  });
});
