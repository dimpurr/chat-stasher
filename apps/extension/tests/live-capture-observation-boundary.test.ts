import { describe, expect, it } from 'vitest';
import {
  HOOK_REASON_DID_NOT_RUN,
} from '../lib/contract';
import {
  captureVerdict,
  CAPTURE_VERDICT_NOT_WORKING,
  CAPTURE_VERDICT_WORKING,
} from '../lib/live-capture';
import type { HookObservationRecord, HookStatusRecord } from '../lib/hook-status';

const SINCE = 1_700_000_000_000;
const PLATFORM = 'chatgpt';
const ORIGIN = 'https://chatgpt.com';

function observationSince(since: number): HookStatusRecord {
  const reason: HookObservationRecord = {
    reason: HOOK_REASON_DID_NOT_RUN,
    at: since + 5_000,
    since,
  };
  return {
    origin: ORIGIN,
    platform: PLATFORM,
    at: reason.at,
    reasons: [reason],
  };
}

describe('live-capture observation boundary', () => {
  it('counts a capture exactly at since, but not one millisecond before it', () => {
    const observed = observationSince(SINCE);

    // A capture is a measurement of the full path, so equality with the
    // observation start is enough to upgrade even a not-in-effect observation.
    expect(captureVerdict(observed, { at: SINCE })).toBe(CAPTURE_VERDICT_WORKING);

    // Just before the boundary it is not evidence about this observation, and
    // the current not-working verdict remains in force.
    expect(captureVerdict(observed, { at: SINCE - 1 })).toBe(CAPTURE_VERDICT_NOT_WORKING);
  });
});
