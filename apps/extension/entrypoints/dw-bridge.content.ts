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
  BACKFILL_TAB_HELLO_MESSAGE,
  handleBackfillMessage,
} from '../lib/backfill/tab-port';
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

    // -----------------------------------------------------------------------
    // 🔴 C19 · This is where the backfill leg's fetch channel lands.
    //
    // This code runs in the context of **the page the user is already logged
    // into**, so the fetch below is a **same-origin** request carrying that
    // page's own cookies — same origin and same credentials as the request the
    // browser sends when the user opens a past conversation by hand. Hence:
    //   · no host permission is needed (same-origin requests are not governed by
    //     host permissions in the first place);
    //   · `matches` need not change a character (it was already injected on these
    //     platforms);
    //   · fetching has not been moved out of the user's logged-in context (the
    //     architectural premise holds).
    // Which URLs may be sent on the page's behalf is decided by the three checks
    // in lib/backfill/tab-port.ts (same origin + in the platform table + only the
    // backfill leg's two paths); nothing is decided here.
    // -----------------------------------------------------------------------
    browser.runtime.onMessage.addListener(
      (message: unknown, _sender: unknown, sendResponse: (r: unknown) => void) => {
        const pending = handleBackfillMessage(message, pageOrigin, async (url, init) => {
          // 🔴 C23: by the time execution reaches here, method / body /
          //    Content-Type have already passed checkBackfillRequest's closed-set
          //    checks (handleBackfillMessage → serveBackfillFetch).
          //    Nothing is decided here, and nothing **may** be — the decision lives
          //    in exactly one place, that allowlist.
          //    init omitted (a GET segment) ⇒ the fetch below takes byte-identical
          //    arguments to C19/C22.
          const res = init && init.method === 'POST'
            ? await fetch(url, {
                method: 'POST',
                credentials: 'same-origin',
                headers: {
                  accept: 'application/json',
                  ...(init.contentType ? { 'content-type': init.contentType } : {}),
                },
                body: init.body,
              })
            : await fetch(url, {
                credentials: 'same-origin',
                headers: { accept: 'application/json' },
              });
          return { status: res.status, text: () => res.text() };
        });
        if (!pending) return;   // not a message for me; leave it to the other listeners
        pending
          .then(sendResponse)
          .catch((err: Error) => sendResponse({ ok: false, error: err.message }));
        return true;            // MV3: an async sendResponse requires returning true
      },
    );

    // Check in: leave the tab id (filled in on `sender` by the browser) with
    // background, so that **when the alarm wakes** it knows which tab to fetch
    // through. A failure is harmless — the live leg uses the sender it has at the
    // time and does not depend on this registry.
    browser.runtime
      .sendMessage({ type: BACKFILL_TAB_HELLO_MESSAGE, origin: pageOrigin })
      .catch(() => { /* background asleep / nobody listening: do not disturb the page */ });

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
