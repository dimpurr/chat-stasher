import { CAPTURE_EVENT, isChatTraffic, MAX_RAW_BYTES, DEEPSEEK_ORIGIN, type CapturedFetch } from '../lib/contract';

/**
 * MAIN-world fetch hook, injected only on chat.deepseek.com.
 * WHY MAIN world: the page's own JS sees a copy of window.fetch even if we
 * patch first — no, actually the opposite: without world:'MAIN' the page's
 * JS (bundled apps keep a reference to the original fetch) would bypass an
 * ISOLATED-world patch. Patching in MAIN runs in the page's own JS context,
 * so DeepSeek's own fetch calls hit our wrapper. That is the capture point.
 */
export default defineContentScript({
  matches: [`${DEEPSEEK_ORIGIN}/*`],
  runAt: 'document_start',
  world: 'MAIN',
  main() {
    const originalFetch = window.fetch.bind(window);

    async function maybeCapture(
      input: RequestInfo | URL,
      method: string,
      response: Response,
    ): Promise<void> {
      try {
        let url: string;
        if (typeof input === 'string') url = input;
        else if (input instanceof URL) url = input.href;
        else url = input.url;
        // Only chat/session traffic on the DeepSeek origin. Everything else
        // passes through un-captured so we stay minimal and non-invasive.
        if (!isChatTraffic(url, method)) return;

        const text = await response.clone().text();
        const bytes = new TextEncoder().encode(text).length;
        // Safety valve: never buffer multi-MB streaming payloads. Anything
        // bigger is media/streaming, not a session JSON we want anyway.
        if (bytes > MAX_RAW_BYTES) {
          console.warn('[chat-stasher] skip oversized capture', bytes);
          return;
        }
        const payload: CapturedFetch = {
          url,
          method,
          status: response.status,
          text,
          capturedAt: Date.now(),
        };
        window.dispatchEvent(
          new CustomEvent<CapturedFetch>(CAPTURE_EVENT, { detail: payload }),
        );
      } catch (err) {
        // Never break the page's own fetch flow because of our logging.
        console.warn('[chat-stasher] capture failed', err);
      }
    }

    window.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const response = await originalFetch(input as RequestInfo, init);
      void maybeCapture(input, init?.method ?? 'GET', response);
      return response; // page behaviour unchanged
    };

    console.log('[chat-stasher] MAIN fetch hook installed on', DEEPSEEK_ORIGIN);
  },
});