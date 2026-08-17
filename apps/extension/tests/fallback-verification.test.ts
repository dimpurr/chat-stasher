import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  FALLBACK_HOOK_VERIFICATION_WARNING,
  isFallbackHookVerified,
  warnIfFallbackHookUnverified,
} from '../lib/fallback-verification';

afterEach(() => {
  vi.restoreAllMocks();
});

describe('fallback page hook verification', () => {
  it('does not treat an appended script as successful when the marker is absent', () => {
    expect(isFallbackHookVerified({ scriptAppended: true, markerInstalled: false })).toBe(false);
    expect(isFallbackHookVerified({ scriptAppended: true, markerInstalled: true })).toBe(true);
  });

  it('uses a fixed warning without response content when marker verification fails', () => {
    const responseContent = '{"secret":"must-not-be-logged"}';
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    expect(FALLBACK_HOOK_VERIFICATION_WARNING).toBe(
      '[chat-stasher] fallback page hook verification failed',
    );
    expect(warnIfFallbackHookUnverified({ scriptAppended: true, markerInstalled: false })).toBe(true);
    expect(warn).toHaveBeenCalledWith(FALLBACK_HOOK_VERIFICATION_WARNING);
    expect(warn.mock.calls[0]).toHaveLength(1);
    expect(warn.mock.calls[0]?.[0] ?? '').not.toContain(responseContent);
    expect(FALLBACK_HOOK_VERIFICATION_WARNING).not.toContain(responseContent);
  });
});
