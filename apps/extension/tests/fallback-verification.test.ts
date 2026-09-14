import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  FALLBACK_HOOK_VERIFICATION_WARNING,
  createFallbackWarningGate,
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

  it('warns at most once per page, however many times the fallback fails to verify', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const gate = createFallbackWarningGate();

    // The two occasions a page can produce: the append itself refused, and then
    // the probe never answered. A page that keeps failing must not become a wall
    // of identical lines — a warning nobody reads is the silence this is for.
    gate({ scriptAppended: false, markerInstalled: false });
    gate({ scriptAppended: true, markerInstalled: false });
    gate({ scriptAppended: true, markerInstalled: false });

    expect(warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING)).toHaveLength(1);
  });

  it('a verified result does not consume the one warning a genuine failure may still need', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const gate = createFallbackWarningGate();

    gate({ scriptAppended: true, markerInstalled: true });
    expect(warn).not.toHaveBeenCalled();

    gate({ scriptAppended: true, markerInstalled: false });
    expect(warn.mock.calls.filter((c) => c[0] === FALLBACK_HOOK_VERIFICATION_WARNING)).toHaveLength(1);
  });
});
