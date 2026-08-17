import {
  CONTENT_MATCHES,
  PLATFORMS,
  MAIN_FALLBACK_TIMEOUT_MS,
  MAIN_PROBE_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_VERSION,
  MAIN_VERIFY_RESULT_MESSAGE,
  isCaptureMessage,
  isMainReadyMessage,
  isMainVerifyResultMessage,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import {
  warnIfFallbackHookUnverified,
  FALLBACK_HOOK_VERIFICATION_WARNING,
} from '../lib/fallback-verification';

/**
 * ISOLATED-world bridge. WHY ISOLATED: MAIN/page world has no extension API
 * access, so this script converts validated page messages into
 * browser.runtime.sendMessage for the background service worker.
 * Matches only the explicit origins in the platform table, never <all_urls>.
 */
export default defineContentScript({
  matches: CONTENT_MATCHES,
  runAt: 'document_start',
  main() {
    const probeToken = makeProbeToken();
    let mainReady = false;
    let fallbackInjectionAttempted = false;
    let fallbackScriptAppended = false;
    let mainVerificationRequested = false;
    let fallbackVerificationRequested = false;
    let fallbackVerificationTimer: ReturnType<typeof setTimeout> | undefined;

    const pageOrigin = window.location.origin;
    const isPageMessage = (event: MessageEvent<unknown>): boolean =>
      event.source === window &&
      event.origin === pageOrigin &&
      PLATFORMS.some((platform) => platform.origins.includes(event.origin));

    const onMessage = (event: MessageEvent<unknown>): void => {
      if (!isPageMessage(event)) return;

      if (isMainReadyMessage(event.data) && event.data.token === probeToken) {
        if (!mainReady && !mainVerificationRequested && !fallbackVerificationRequested) {
          mainVerificationRequested = true;
          if (!injectPageVerifier(probeToken)) {
            mainVerificationRequested = false;
            injectFallbackHook();
          }
        }
        return;
      }

      if (isMainVerifyResultMessage(event.data) && event.data.token === probeToken) {
        if (fallbackVerificationRequested) {
          fallbackVerificationRequested = false;
          if (fallbackVerificationTimer !== undefined) {
            clearTimeout(fallbackVerificationTimer);
            fallbackVerificationTimer = undefined;
          }
          if (
            warnIfFallbackHookUnverified({
              scriptAppended: fallbackScriptAppended,
              markerInstalled: event.data.installed,
            })
          ) {
            return;
          }
          mainReady = true;
          return;
        }

        if (!mainVerificationRequested) return;
        mainVerificationRequested = false;
        if (event.data.installed) {
          mainReady = true;
        } else {
          injectFallbackHook();
        }
        return;
      }

      if (!isCaptureMessage(event.data)) return;
      browser.runtime
        .sendMessage({ type: 'chat-captured', payload: event.data.payload })
        .catch(() => {
          // A failed extension channel must never disturb the page.
        });
    };

    function injectPageScript(source: string): boolean {
      if (typeof document === 'undefined') return false;
      const parent = document.documentElement ?? document.head;
      if (!parent) return false;
      const script = document.createElement('script');
      script.textContent = source;
      parent.appendChild(script);
      script.remove();
      return true;
    }

    function injectPageVerifier(token: string): boolean {
      const marker = JSON.stringify(PAGE_HOOK_FETCH_MARKER);
      const version = JSON.stringify(PAGE_HOOK_VERSION);
      const origin = JSON.stringify(pageOrigin);
      return injectPageScript(`(() => {
        const fetchFn = window.fetch;
        const installed = typeof fetchFn === 'function' && fetchFn[${marker}] === ${version};
        window.postMessage({
          type: ${JSON.stringify(MAIN_VERIFY_RESULT_MESSAGE)},
          version: ${version},
          token: ${JSON.stringify(token)},
          installed,
        }, ${origin});
      })();`);
    }

    function injectFallbackHook(): void {
      if (fallbackInjectionAttempted || mainReady) return;
      fallbackInjectionAttempted = true;
      const source = `(${installPageFetchHook.toString()})(${JSON.stringify(PAGE_HOOK_OPTIONS)});`;
      fallbackScriptAppended = injectPageScript(source);
      if (!fallbackScriptAppended) {
        console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
        return;
      }

      fallbackVerificationRequested = true;
      if (!injectPageVerifier(probeToken)) {
        fallbackVerificationRequested = false;
        console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
        return;
      }
      fallbackVerificationTimer = setTimeout(() => {
        if (!fallbackVerificationRequested) return;
        fallbackVerificationRequested = false;
        fallbackVerificationTimer = undefined;
        console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
      }, MAIN_FALLBACK_TIMEOUT_MS);
    }

    window.addEventListener('message', onMessage);
    // A tokenized probe makes the readiness handshake insensitive to which
    // document_start content script runs first.
    window.postMessage({ type: MAIN_PROBE_MESSAGE, token: probeToken }, pageOrigin);
    setTimeout(() => {
      if (!mainReady && !mainVerificationRequested && !fallbackVerificationRequested) {
        injectFallbackHook();
      }
    }, MAIN_FALLBACK_TIMEOUT_MS);
  },
});

function makeProbeToken(): string {
  try {
    return crypto.randomUUID();
  } catch {
    return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  }
}
