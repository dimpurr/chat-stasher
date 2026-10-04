import { describe, expect, it } from 'vitest';
import { HOOK_REASON_DID_NOT_RUN, HOOK_REASON_WAS_REPLACED } from '../lib/contract';
import {
  hookStatusKey,
  hookStatusOf,
  looksLikeHookStatus,
  type HookStatusRecord,
} from '../lib/hook-status';

const ORIGIN = 'https://chatgpt.com';

function record(overrides: Partial<HookStatusRecord> = {}): HookStatusRecord {
  return {
    origin: ORIGIN,
    platform: 'chatgpt',
    reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at: 5_000, since: 4_000 }],
    at: 5_000,
    ...overrides,
  };
}

describe('W474 · persisted hook-status validation', () => {
  it('keeps a valid legacy record whose reason has no since field', () => {
    const legacy = record({ reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at: 5_000 }] });

    expect(looksLikeHookStatus(legacy)).toBe(true);
    expect(hookStatusOf({ [hookStatusKey(ORIGIN)]: legacy })).toEqual([legacy]);
  });

  it('omits non-finite and negative record, observation, and since timestamps', () => {
    const invalid = [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, -1];
    const rows = Object.fromEntries(invalid.flatMap((at, index) => {
      const origin = `https://synthetic-${index}.example`;
      const value = record({ origin, at });
      return [[hookStatusKey(origin), value]];
    }));
    const badObservationTimes = Object.fromEntries(invalid.flatMap((at, index) => {
      const origin = `https://synthetic-observation-${index}.example`;
      const value = record({ origin, reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at }] });
      return [[hookStatusKey(origin), value]];
    }));
    const badSinceTimes = Object.fromEntries(invalid.flatMap((since, index) => {
      const origin = `https://synthetic-since-${index}.example`;
      const value = record({
        origin,
        reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at: 5_000, since }],
      });
      return [[hookStatusKey(origin), value]];
    }));
    const snapshot = { ...rows, ...badObservationTimes, ...badSinceTimes };

    expect(hookStatusOf(snapshot)).toEqual([]);
  });

  it('omits duplicate rows for one reason instead of showing either timestamp as current', () => {
    const duplicate = record({
      reasons: [
        { reason: HOOK_REASON_DID_NOT_RUN, at: 4_000 },
        { reason: HOOK_REASON_DID_NOT_RUN, at: 5_000 },
      ],
    });

    expect(looksLikeHookStatus(duplicate)).toBe(false);
    expect(hookStatusOf({ [hookStatusKey(ORIGIN)]: duplicate })).toEqual([]);
  });

  it('omits records whose storage key disagrees with the embedded origin', () => {
    const embedded = record({ origin: 'https://gemini.google.com', platform: 'gemini' });

    expect(hookStatusOf({ [hookStatusKey(ORIGIN)]: embedded })).toEqual([]);
  });

  it('keeps distinct valid origins as distinct diagnostics, newest first', () => {
    const older = record({ at: 4_000 });
    const newer = record({
      origin: 'https://gemini.google.com',
      platform: 'gemini',
      reasons: [{ reason: HOOK_REASON_DID_NOT_RUN, at: 6_000 }],
      at: 6_000,
    });

    expect(hookStatusOf({
      [hookStatusKey(older.origin)]: older,
      [hookStatusKey(newer.origin)]: newer,
    })).toEqual([newer, older]);
  });
});
