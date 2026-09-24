import { CONTENT_MATCHES } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

/**
 * MAIN-world fetch hook, injected only on the explicit platform origins.
 * WHY MAIN world: the page's own JS sees a copy of window.fetch even if we
 * patch first — no, actually the opposite: without world:'MAIN' the page's
 * JS (bundled apps keep a reference to the original fetch) would bypass an
 * ISOLATED-world patch. Patching in MAIN runs in the page's own JS context,
 * so DeepSeek's own fetch calls hit our wrapper. That is the capture point.
 */
export default defineContentScript({
  matches: CONTENT_MATCHES,
  runAt: 'document_start',
  world: 'MAIN',
  /**
   * 🔴 W36b · **And in every subframe of those origins.**
   *
   * `allFrames` defaults to false, and until W36b that left every same-origin
   * iframe of a matched origin with the browser's own `window.fetch` and
   * `XMLHttpRequest.prototype.open`, its request going out and coming back, and
   * **not one capture message posted on that frame's own window** — measured in
   * a real Chromium on this branch (`e2e/frame-capture.spec.ts`), with the top
   * document's hook installed and working in the same run.
   *
   * 🔴 **What that fixture reproduces, and what it does not.** It reproduces the
   *    *reading*: a document on a matched origin, same-origin with the top
   *    document, that the hook never reached. It does **not** reproduce the two
   *    tabs the first real-Chrome acceptance measured, and must not be read as
   *    saying so. That measurement on 2026-09-19 was the browser's own `fetch`
   *    and `XHR.prototype.open` in the document that was on screen — nothing in
   *    it establishes that the document was a frame — and the second tab's
   *    remaining explanation is that the wrapper the page showed may have been
   *    the site's own. Which of those shapes those tabs were is still not
   *    settled, and this fixture does not settle it.
   *
   *    The frame case is pinned here because it is the one shape of "the hook is
   *    not in the document that made the request" the extension can fix: a
   *    document that predates the extension cannot be re-injected into (Chrome
   *    offers no way without host permissions, which this manifest deliberately
   *    does not take), but a frame the browser was never asked to inject into is
   *    a line of configuration.
   *
   * The bound is the one CONTENT_MATCHES already draws — the origins of
   * lib/contract.ts's platform table for the active release channel (six in a
   * stable build, eight in a dev one), never a wildcard — so the extra cost is a
   * second copy of this script and of the bridge in same-origin iframes of those
   * hosts only. `e2e/frame-capture.spec.ts` is the test that fails without it.
   */
  allFrames: true,
  matchOriginAsFallback: true,
  main() {
    installPageFetchHook(PAGE_HOOK_OPTIONS);
  },
});
