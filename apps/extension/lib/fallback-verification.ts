export const FALLBACK_HOOK_VERIFICATION_WARNING =
  '[chat-stasher] fallback page hook verification failed';

export interface FallbackHookVerification {
  scriptAppended: boolean;
  markerInstalled: boolean;
}

export function isFallbackHookVerified(result: FallbackHookVerification): boolean {
  return result.scriptAppended && result.markerInstalled;
}

export function warnIfFallbackHookUnverified(result: FallbackHookVerification): boolean {
  if (isFallbackHookVerified(result)) return false;
  console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
  return true;
}
