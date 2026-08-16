import { CAPTURE_EVENT, DEEPSEEK_ORIGIN, type CapturedFetch } from '../lib/contract';

/**
 * ISOLATED-world bridge. WHY ISOLATED: MAIN world has no extension API
 * access, so this script converts the page-level CustomEvent into
 * browser.runtime.sendMessage for the background service worker.
 * Matches only chat.deepseek.com, never <all_urls>.
 */
export default defineContentScript({
  matches: [`${DEEPSEEK_ORIGIN}/*`],
  runAt: 'document_start',
  main() {
    window.addEventListener(CAPTURE_EVENT, (event: Event) => {
      const detail = (event as CustomEvent<CapturedFetch>).detail;
      browser.runtime
        .sendMessage({ type: 'chat-captured', payload: detail })
        .catch((err: unknown) => {
          // Bridge must never disturb the page; a failed channel is only logged.
          console.warn('[chat-stasher] bridge send failed', (err as Error).message);
        });
    });
    console.log('[chat-stasher] ISOLATED bridge listening on', CAPTURE_EVENT);
  },
});