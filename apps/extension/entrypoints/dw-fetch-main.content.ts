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
  main() {
    installPageFetchHook(PAGE_HOOK_OPTIONS);
  },
});
