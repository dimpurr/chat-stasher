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

/**
 * The same warning, at most once per page.
 *
 * 🔴 Why the cap is part of the semantics and not a detail of the caller: the
 *    fallback can fail to verify on more than one occasion in a page's life (the
 *    append itself refused, then the token probe never answered), and a page
 *    that keeps failing is exactly the page that would otherwise produce a wall
 *    of identical lines. The signal has to stay legible or it stops being read —
 *    and a warning nobody reads is indistinguishable from the silence this
 *    capability exists to remove.
 *    A *verified* result does not consume the one warning: it is not a failure,
 *    so a genuine failure later still gets to be the one that speaks.
 */
export function createFallbackWarningGate(): (result: FallbackHookVerification) => void {
  let warned = false;
  return (result: FallbackHookVerification): void => {
    if (warned) return;
    if (warnIfFallbackHookUnverified(result)) warned = true;
  };
}
