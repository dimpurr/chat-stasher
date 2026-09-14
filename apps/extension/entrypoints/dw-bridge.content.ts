import {
  CONTENT_MATCHES,
  PLATFORMS,
  MAIN_FALLBACK_TIMEOUT_MS,
  MAIN_PROBE_MESSAGE,
  isCaptureMessage,
  isMainReadyMessage,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import {
  BACKFILL_TAB_HELLO_MESSAGE,
  handleBackfillMessage,
  serveBackfillFetch,
  type FetchLike,
} from '../lib/backfill/tab-port';
import {
  chatgptDetailUrlFor,
  createAuthorizedFetch,
  createSeenGate,
  isConversationSeenMessage,
} from '../lib/platform-auth';
import { createFallbackWarningGate } from '../lib/fallback-verification';

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
    let fallbackVerificationRequested = false;
    let fallbackVerificationTimer: ReturnType<typeof setTimeout> | undefined;
    /** 🔴 At most one fallback warning per page; see lib/fallback-verification.ts. */
    const warnFallbackUnverified = createFallbackWarningGate();

    const pageOrigin = window.location.origin;
    const isPageMessage = (event: MessageEvent<unknown>): boolean =>
      event.source === window &&
      event.origin === pageOrigin &&
      PLATFORMS.some((platform) => platform.origins.includes(event.origin));

    const onMessage = (event: MessageEvent<unknown>): void => {
      if (!isPageMessage(event)) return;

      if (isMainReadyMessage(event.data) && event.data.token === probeToken) {
        // 🔴 This answer **is** the verification, and it is why no inline script
        //    is injected any more. `installPageFetchHook` registers the listener
        //    that produces it only after `window.fetch` has been replaced
        //    (lib/page-hook.ts), so a token this handshake invented, echoed back
        //    from the page world, says the hook is installed there. The <script>
        //    this replaces read the same fact off `window.fetch` by hand — and on
        //    chat.deepseek.com the page's `script-src` refuses to execute it, so
        //    every page load filed a CSP violation while proving nothing that the
        //    probe had not already proved.
        if (fallbackVerificationRequested) {
          fallbackVerificationRequested = false;
          clearFallbackVerificationTimer();
          // Reported through the gate rather than assumed: a hook that answered
          // is a hook that verified, so this is a no-op. It is routed here so
          // that "verified or not" stays one decision in one module instead of
          // being re-derived at each call site.
          warnFallbackUnverified({ scriptAppended: fallbackScriptAppended, markerInstalled: true });
        }
        mainReady = true;
        return;
      }

      if (isConversationSeenMessage(event.data)) {
        const id = event.data.id;
        if (!seenGate(id)) return;
        void refetchFullConversation(id);
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

    function clearFallbackVerificationTimer(): void {
      if (fallbackVerificationTimer === undefined) return;
      clearTimeout(fallbackVerificationTimer);
      fallbackVerificationTimer = undefined;
    }

    function injectFallbackHook(): void {
      if (fallbackInjectionAttempted || mainReady) return;
      fallbackInjectionAttempted = true;
      const source = `(${installPageFetchHook.toString()})(${JSON.stringify(PAGE_HOOK_OPTIONS)});`;
      fallbackScriptAppended = injectPageScript(source);
      if (!fallbackScriptAppended) {
        warnFallbackUnverified({ scriptAppended: false, markerInstalled: false });
        return;
      }

      // 🔴 The fallback script is an `installPageFetchHook` instance, so if it
      //    really ran it answers this very probe from its own listener; silence
      //    means it did not. That distinction is the one thing an `appendChild`
      //    return value cannot carry, because appending an inline <script>
      //    succeeds even when the page's CSP refuses to execute it — the
      //    fallback's whole failure mode on chat.deepseek.com. So we ask the page
      //    world rather than inspecting it, which is also why this path no longer
      //    needs a <script> of its own to check the first one.
      fallbackVerificationRequested = true;
      window.postMessage({ type: MAIN_PROBE_MESSAGE, token: probeToken }, pageOrigin);
      fallbackVerificationTimer = setTimeout(() => {
        if (!fallbackVerificationRequested) return;
        fallbackVerificationRequested = false;
        fallbackVerificationTimer = undefined;
        warnFallbackUnverified({ scriptAppended: fallbackScriptAppended, markerInstalled: false });
      }, MAIN_FALLBACK_TIMEOUT_MS);
    }

    // The one fetch both legs use. ChatGPT body requests get the session's
    // bearer token (in memory only; lib/platform-auth.ts); every other request
    // is sent exactly as before.
    const authorizedFetch = createAuthorizedFetch(pageOrigin, (url, init) => fetch(url, init));
    const pageFetch: FetchLike = async (url, init) => {
      // 🔴 C23: by the time execution reaches here, method / body /
      //    Content-Type have already passed checkBackfillRequest's closed-set
      //    checks (serveBackfillFetch). Nothing is decided here, and nothing
      //    **may** be — the decision lives in exactly one place, that allowlist.
      const res = init && init.method === 'POST'
        ? await authorizedFetch(url, {
            method: 'POST',
            credentials: 'same-origin',
            headers: {
              accept: 'application/json',
              ...(init.contentType ? { 'content-type': init.contentType } : {}),
            },
            body: init.body,
          })
        : await authorizedFetch(url, {
            credentials: 'same-origin',
            headers: { accept: 'application/json' },
          });
      return { status: res.status, text: () => res.text() };
    };

    // Live leg for ChatGPT's paged navigation: fetch the full conversation
    // through the same allowlist the backfill leg uses, then hand it to
    // background exactly like a passive capture (same shape checks, outbox, ack).
    const seenGate = createSeenGate(15_000);
    async function refetchFullConversation(id: string): Promise<void> {
      const url = chatgptDetailUrlFor(pageOrigin, id);
      const reply = await serveBackfillFetch(url, pageOrigin, pageFetch);
      if (!reply.ok || reply.status < 200 || reply.status > 299) {
        // Metadata only: the HTTP status or the technical refusal reason —
        // never the URL, id, token, or body.
        const why = reply.ok ? `HTTP ${reply.status}` : reply.error;
        console.warn(`[chat-stasher] full-conversation refetch failed (${why}); this navigation was not captured`);
        return;
      }
      browser.runtime
        .sendMessage({
          type: 'chat-captured',
          payload: {
            url,
            method: 'GET',
            status: reply.status,
            text: reply.text,
            pageUrl: window.location.href,
            capturedAt: Date.now(),
          },
        })
        .catch(() => {
          // A failed extension channel must never disturb the page.
        });
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
        const pending = handleBackfillMessage(message, pageOrigin, pageFetch);
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
    // document_start content script runs first, and its answer is the whole of
    // the readiness decision — nothing else is injected to look at the page.
    window.postMessage({ type: MAIN_PROBE_MESSAGE, token: probeToken }, pageOrigin);
    setTimeout(() => {
      if (!mainReady && !fallbackVerificationRequested) {
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
