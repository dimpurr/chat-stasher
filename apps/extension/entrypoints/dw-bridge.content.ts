import {
  CONTENT_MATCHES,
  PLATFORMS,
  MAIN_FALLBACK_TIMEOUT_MS,
  MAIN_PROBE_MESSAGE,
  GEMINI_TOKENS_REQUEST_MESSAGE,
  findPlatformForUrl,
  isCaptureMessage,
  isGeminiTokensReply,
  isMainReadyMessage,
  type CapturedFetch,
  type GeminiBootstrapTokens,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import {
  BACKFILL_TAB_HELLO_MESSAGE,
  handleBackfillMessage,
  serveBackfillFetch,
  type FetchLike,
} from '../lib/backfill/tab-port';
import { installTabHello } from '../lib/backfill/tab-hello';
import { backfillPlanFor } from '../lib/backfill/enumerate';
import { createClaudePageScope } from '../lib/backfill/claude-page';
import {
  chatgptDetailUrlFor,
  createAuthorizedFetch,
  createGeminiAuthorizedFetch,
  createKimiAuthorizedFetch,
  createSeenGate,
  isConversationSeenMessage,
  KIMI_ACCESS_TOKEN_STORAGE_KEY,
  GEMINI_TOKEN_PULL_TIMEOUT_MS,
} from '../lib/platform-auth';
import { completeGeminiLiveCapture, isGeminiDetailRequest } from '../lib/gemini-capture';
import { createFallbackWarningGate } from '../lib/fallback-verification';
import { createStaleLinkWarningGate } from '../lib/page-link';

/**
 * ISOLATED-world bridge. WHY ISOLATED: MAIN/page world has no extension API
 * access, so this script converts validated page messages into
 * browser.runtime.sendMessage for the background service worker.
 * Matches only the explicit origins in the platform table, never <all_urls>.
 */
export default defineContentScript({
  matches: CONTENT_MATCHES,
  runAt: 'document_start',
  /**
   * 🔴 W36b · **Every subframe, because a capture is relayed from the frame that
   *    made the request.**
   *
   * `dw-fetch-main.content.ts` now installs its hook in subframes too, and the
   * hook posts its capture to **its own window** — so without this the patch would
   * be installed and the message would go nowhere (measured: a same-origin iframe
   * with the hook absent posts nothing at all, and with the hook present but no
   * bridge here posts the same nothing).
   *
   * 🔴 The tab-level half of this file is **top-frame only** (`isTopFrame` below):
   *    the backfill fetch channel and the periodic hello belong to the tab, not to
   *    each of its frames. `browser.tabs.sendMessage(tabId, …)` with no `frameId`
   *    is delivered to every frame in the tab and settles with the first answer,
   *    so a listener in every frame would put several answers in a race for one
   *    request. The capture path itself is per-frame by nature and stays in every
   *    frame.
   */
  allFrames: true,
  main() {
    const probeToken = makeProbeToken();
    let mainReady = false;
    let fallbackInjectionAttempted = false;
    let fallbackScriptAppended = false;
    let fallbackVerificationRequested = false;
    let fallbackVerificationTimer: ReturnType<typeof setTimeout> | undefined;
    /** 🔴 At most one fallback warning per page; see lib/fallback-verification.ts. */
    const warnFallbackUnverified = createFallbackWarningGate();
    /** 🔴 W36 · At most one stale-link warning per page; see lib/page-link.ts. */
    const warnStaleLink = createStaleLinkWarningGate();

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
      // 🔴 W31c · The page's own requests are the resolver's first and strongest
      //    source of "which organization is this page using" — see
      //    lib/backfill/claude-page.ts. Recorded **before** delivery, so it is
      //    remembered even when the delivery path itself bails out.
      claudePage.rememberRequest(event.data.payload.url);
      void deliverCapture(event.data.payload);
    };

    /**
     * 🔴 W29 · **The one place a capture can be completed before it is archived.**
     *
     * For Gemini, a page load is **not** the conversation: its body RPC is paged,
     * and the response the hook saw may be any page of it (the page asks for older
     * turns as the user scrolls). So the conversation is fetched from its first
     * page here — through `serveBackfillFetch`, i.e. the very same allowlist the
     * backfill leg goes through — and what is delivered is the assembled bundle.
     * lib/gemini-capture.ts states the reasoning, including why the observed
     * response is used for its identity and not as the bundle's first page. Every
     * other payload is delivered exactly as it arrived, byte for byte.
     *
     * 🔴 A conversation that cannot be completed is **not delivered at all**. The
     *    pages in hand are real content and still not the conversation, so storing
     *    them would be archiving a partial answer as a whole one; the backfill leg
     *    will try that conversation again. The warning carries the reason and
     *    nothing else — never the response, never an id.
     */
    async function deliverCapture(payload: CapturedFetch): Promise<void> {
      let outgoing = payload;
      if (findPlatformForUrl(payload.url)?.id === 'gemini' && isGeminiDetailRequest(payload.url)) {
        const completed = await completeGeminiLiveCapture(payload, {
          pageOrigin,
          fetchPage: (url, init) => serveBackfillFetch(
            { url, method: init.method, body: init.body, contentType: init.contentType },
            pageOrigin,
            pageFetch,
          ),
          sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
          random: Math.random,
        });
        if (!completed.ok) {
          console.warn(
            `[chat-stasher] gemini conversation not captured: ${completed.reason}`,
          );
          return;
        }
        outgoing = completed.payload;
      }
      browser.runtime
        .sendMessage({ type: 'chat-captured', payload: outgoing })
        .catch((err: unknown) => {
          // 🔴 W36 · An empty catch here is how a real conversation disappears
          //    without a trace: on a page left open across an extension reload
          //    this document's scripts are the previous build's, so the capture
          //    is produced (the page-world hook is still installed) and then
          //    dropped against a context that no longer exists. The page is
          //    never disturbed — that rule stands — but the drop is **named**,
          //    once per page. See lib/page-link.ts.
          warnStaleLink(err);
        });
    }

    /**
     * 🔴 W36 · **A refusal is a value here, never an exception.**
     *
     * Building the script is three statements that can each be refused by the
     * page, and only one of them used to be survivable:
     *  · `document.createElement` — a page can be gone (the `typeof document`
     *    guard above) or have no root element;
     *  · **`script.textContent = source` — a Trusted Types page throws here.**
     *    Measured 2026-09-19 in a real Chromium on a page served with
     *    `require-trusted-types-for 'script'`:
     *      TypeError: Failed to set the 'textContent' property on
     *      'HTMLScriptElement': This document requires 'TrustedScript' assignment.
     *    `HTMLScriptElement.text` is a TrustedScript sink, so on such a page the
     *    assignment is refused *before* anything is appended or executed.
     *    🔴 W36b · **This is the FALLBACK being refused, and it is not what the
     *    first acceptance measured.** The acceptance saw a `gemini.google.com/app/<id>`
     *    document whose `window.fetch` and `XMLHttpRequest.prototype.open` were
     *    both the browser's own — which means no hook was installed there by any
     *    route, and a page's CSP has no say over the **declarative** MAIN-world
     *    registration in the first place (`e2e/main-world-hook.spec.ts` serves
     *    exactly that CSP and the declarative hook installs and captures on it).
     *    A refused assignment here is silent, and silence is what this change
     *    fixes; it is not an explanation of that tab. See W36b's notes on
     *    `e2e/frame-capture.spec.ts` for the two shapes that are;
     *  · `appendChild` — the CSP case the W12 comment below describes (appended,
     *    never executed).
     *
     * Swallowing the exception is not the point; **reporting it is**. The return
     * value is what the caller's verification reads, so `false` means "we could
     * not get the fallback into this page" and the fixed warning — the one
     * signal this whole capability has — fires. Losing the exception object is
     * deliberate: its message is the browser's, and the warning is fixed and
     * metadata-only by design (lib/fallback-verification.ts).
     */
    function injectPageScript(source: string): boolean {
      if (typeof document === 'undefined') return false;
      const parent = document.documentElement ?? document.head;
      if (!parent) return false;
      try {
        const script = document.createElement('script');
        script.textContent = source;
        parent.appendChild(script);
        script.remove();
      } catch {
        return false;
      }
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

    /**
     * 🔴 W22 · The page origin's own Kimi token, read at the moment a request is
     * about to be made and returned to exactly one caller. It is never stored on
     * this side, never logged, and never put into a message.
     *
     * 🔴 `null` covers both "the key is not there" and "the storage is
     *    unreadable" (a partitioned or blocked origin throws on access). Those are
     *    two different facts, but **not to this function**: it is not the place
     *    that reports them. What matters downstream is that neither is turned into
     *    an empty result — the request goes out without the token and the
     *    platform's own 401 is what the leg sees (lib/platform-auth.ts).
     */
    function readKimiAccessToken(): string | null {
      try {
        return window.localStorage.getItem(KIMI_ACCESS_TOKEN_STORAGE_KEY);
      } catch {
        return null;
      }
    }

    /**
     * 🔴 W29 · **Gemini's bootstrap tokens, asked for through the page world.**
     *
     * The values live in the page's `WIZ_global_data`, which a content script
     * cannot see, so the MAIN-world hook is asked (lib/page-hook.ts) and answers
     * with exactly the three slots the platform row names. One `postMessage` in
     * and one back; nothing is cached here, nothing is stored, nothing is logged,
     * and no value is ever put into a message to background or to the host.
     *
     * 🔴 What crosses is what the page already holds — every script on this page
     *    can read `window.WIZ_global_data` directly — so this pull discloses
     *    nothing to the page. It is also why no nonce guards the reply: there is
     *    no secret to protect, and the only thing a forged reply could do is make
     *    the next request fail against the same origin it was already going to.
     *
     * `null` covers "the hook did not answer" and "the page has no such blob"
     * alike, and both mean the same thing downstream: the request goes out
     * without the values and the platform's own 400 is what the leg records.
     */
    function pullGeminiTokens(): Promise<GeminiBootstrapTokens | null> {
      return new Promise((resolve) => {
        let settled = false;
        const finish = (value: GeminiBootstrapTokens | null): void => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          window.removeEventListener('message', onReply);
          resolve(value);
        };
        const onReply = (event: MessageEvent<unknown>): void => {
          if (event.source !== window || event.origin !== pageOrigin) return;
          if (!isGeminiTokensReply(event.data)) return;
          finish({ at: event.data.at, bl: event.data.bl, fSid: event.data.fSid });
        };
        const timer = setTimeout(() => finish(null), GEMINI_TOKEN_PULL_TIMEOUT_MS);
        window.addEventListener('message', onReply);
        window.postMessage({ type: GEMINI_TOKENS_REQUEST_MESSAGE }, pageOrigin);
      });
    }

    // The one fetch both legs use. ChatGPT body requests get the session's
    // bearer token (in memory only; lib/platform-auth.ts); every other request
    // is sent exactly as before.
    const chatgptFetch = createAuthorizedFetch(pageOrigin, (url, init) => fetch(url, init));
    // 🔴 W22 · Kimi's two backfill paths need the page origin's own
    //    `access_token` as a bearer token. The wrapper reads it from localStorage
    //    **at request time**, keeps it in no variable of its own, attaches it only
    //    to those two paths, and passes every other request — ChatGPT's included —
    //    straight through to the wrapper above. Nothing about it is decided here.
    const kimiFetch = createKimiAuthorizedFetch(pageOrigin, chatgptFetch, {
      readToken: readKimiAccessToken,
      language: typeof navigator === 'undefined' ? null : navigator.language,
    });
    // 🔴 W29 · Gemini's `batchexecute` requests carry three of the page's own
    //    values: `at` in the body, `bl` and `f.sid` (plus the page's language and
    //    its request counter) in the query. The wrapper reads them per request
    //    through the pull above, rebuilds the query and the body from what it
    //    just read, attaches them only to those two rpcids, and passes every
    //    other request — ChatGPT's and Kimi's included — straight through.
    const authorizedFetch = createGeminiAuthorizedFetch(pageOrigin, kimiFetch, {
      readTokens: pullGeminiTokens,
      language: typeof navigator === 'undefined' ? null : navigator.language,
    });
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

    /**
     * 🔴 W31c · **The page-side half of the organization resolver** — the caller
     * `lib/backfill/claude-org.ts` did not have.
     *
     * It lives in its own module (lib/backfill/claude-page.ts) rather than here, and
     * that is a deliberate choice about what can be tested: this file boots against
     * a real page and cannot be driven under node, so glue written inline here would
     * be reachable and unverifiable at the same time. What stays here is the wiring
     * — the three facts it cannot get for itself (the origin, this page's fetch, and
     * `document.cookie`), the message it answers, and the scope the fetch channel
     * below is handed.
     */
    const claudePage = createClaudePageScope({
      pageOrigin,
      fetchImpl: pageFetch,
      // 🔴 Read **at ask time**, never cached: a tab that has switched organizations
      //    must not be answered from a cookie value taken before the switch.
      readCookie: () => (typeof document === 'undefined' ? null : document.cookie),
    });

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
        .catch((err: unknown) => {
          // W36 · The same rule as deliverCapture: never disturb the page, never
          // lose the fact. See lib/page-link.ts.
          warnStaleLink(err);
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
    // 🔴 W36b · The tab-level channel: the top document of the tab, and only it.
    //    See the `allFrames` note at the top of this file — a listener in every
    //    frame would answer one `tabs.sendMessage` several times over.
    if (isTopFrame()) browser.runtime.onMessage.addListener(
      (message: unknown, _sender: unknown, sendResponse: (r: unknown) => void) => {
        // 🔴 W31c · The organization question, asked **before** the fetch channel
        //    below only because it is a different kind of thing: it carries no URL
        //    and no body, and its answer is a decision. Both paths return null for a
        //    message that is not theirs, so the order between them decides nothing.
        const orgPending = claudePage.handleMessage(message);
        if (orgPending) {
          orgPending
            .then(sendResponse)
            .catch((err: Error) => sendResponse({ ok: false, halt: 'transport-error', detail: err.message }));
          return true;
        }
        // 🔴 W31c · The fifth argument is the scope **this page is allowed to
        //    address**. For a plan whose paths carry an account scope (claude.ai)
        //    the allowlist compares the `{org}` path segment against it and refuses
        //    a request naming another organization — and refuses every request at
        //    all when it is null, which is why a resolved organization alone would
        //    not have been enough. Every other platform's plan declares no scope, so
        //    the value is not read for them and their URLs are unchanged.
        const pending = handleBackfillMessage(
          message, pageOrigin, pageFetch, backfillPlanFor, claudePage.allowedScope(),
        );
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
    // 🔴 W27 · And **keep** checking in. One hello per page load was not enough:
    //    reloading the extension tears this script down, and nothing puts the tab
    //    back until the user reloads the page, so the tick reported 'no-http-port'
    //    with the site wide open on screen. installTabHello repeats the same
    //    message on a jittered interval and when the tab becomes visible; the
    //    cadence, and why it is a plain page timer rather than an alarm, are in
    //    lib/backfill/tab-hello.ts. Background dedups by tab id, so repeats cost
    //    one message and change nothing else.
    // 🔴 W36b · The hello is the tab's, not each frame's: background dedups by tab
    //    id, so a hello from every frame would cost N messages to say one thing —
    //    and on a page left open across an extension reload it would name the same
    //    stale link N times.
    if (isTopFrame()) {
      installTabHello({
        hello: () => browser.runtime.sendMessage({ type: BACKFILL_TAB_HELLO_MESSAGE, origin: pageOrigin }),
        // A test running under node has no `document`; the page always does.
        visibility: typeof document === 'undefined' ? null : document,
        // 🔴 W36 · This is the occasion a page with no traffic gets: the hello
        //    repeats every few minutes, and on a document whose scripts predate the
        //    current build every one of them fails the same way. Naming it here is
        //    what turns "no-http-port for days while the site is open on screen"
        //    into a sentence the developer can act on.
        onFailure: (err) => { warnStaleLink(err); },
      });
    }

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

/**
 * 🔴 W36b · **Is this the tab's top document?**
 *
 * The question the two gates above ask. `window.top` is the one fact a content
 * script can read about its own frame without any permission, and comparing it to
 * `window` is a comparison of two same-origin references, so it cannot throw even
 * across origins.
 *
 * 🔴 A context with **no `top` at all** is not a nested browsing context — the
 *    node test environment's stub window is the only such place, and it stands in
 *    for the top document. Treating it as a subframe would silently disable the
 *    backfill channel in the one environment that exercises it under node.
 */
function isTopFrame(): boolean {
  const self = window as unknown as { top?: unknown };
  return self.top === undefined || self.top === window;
}

function makeProbeToken(): string {
  try {
    return crypto.randomUUID();
  } catch {
    return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  }
}
