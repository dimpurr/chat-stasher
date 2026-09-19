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
   * iframe of a matched origin in exactly the state the first real-Chrome
   * acceptance measured on `gemini.google.com/app/<id>` and `www.kimi.com/chat/<id>`:
   * `window.fetch` and `XMLHttpRequest.prototype.open` are the browser's own, the
   * page's request goes out and returns, and **not one capture message is posted**
   * — measured in a real Chromium on this branch (`e2e/frame-capture.spec.ts`),
   * with the top document's hook installed and working in the same run. It is the
   * one shape of "the hook is not in the document that made the request" that the
   * extension can actually fix: a document that predates the extension cannot be
   * re-injected into (Chrome offers no way without host permissions, which this
   * manifest deliberately does not take), but a frame the browser was never asked
   * to inject into is a line of configuration.
   *
   * The bound is the one CONTENT_MATCHES already draws — the eight origins of
   * lib/contract.ts's platform table, never a wildcard — so the extra cost is a
   * second copy of this script and of the bridge in same-origin iframes of those
   * hosts only. `e2e/frame-capture.spec.ts` is the test that fails without it.
   */
  allFrames: true,
  main() {
    installPageFetchHook(PAGE_HOOK_OPTIONS);
  },
});
