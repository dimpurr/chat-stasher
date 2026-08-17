import {
  CAPTURE_MESSAGE,
  CHAT_PATH_HINTS,
  DEEPSEEK_ORIGIN,
  MAX_RAW_BYTES,
  MAIN_PROBE_MESSAGE,
  MAIN_READY_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_STATE_KEY,
  PAGE_HOOK_VERSION,
} from './contract';

/** Values are serialized into the fallback <script>; no extension API belongs here. */
export interface PageHookOptions {
  captureMessage: string;
  probeMessage: string;
  readyMessage: string;
  stateKey: string;
  fetchMarkerKey: string;
  version: string;
  targetOrigin: string;
  chatPathHints: string[];
  maxRawBytes: number;
}

export const PAGE_HOOK_OPTIONS: PageHookOptions = {
  captureMessage: CAPTURE_MESSAGE,
  probeMessage: MAIN_PROBE_MESSAGE,
  readyMessage: MAIN_READY_MESSAGE,
  stateKey: PAGE_HOOK_STATE_KEY,
  fetchMarkerKey: PAGE_HOOK_FETCH_MARKER,
  version: PAGE_HOOK_VERSION,
  targetOrigin: DEEPSEEK_ORIGIN,
  chatPathHints: [...CHAT_PATH_HINTS],
  maxRawBytes: MAX_RAW_BYTES,
};

/**
 * Page-world-only hook. MAIN and the injected fallback call this exact function,
 * so the global state and fetch marker are identical on both paths.
 */
export function installPageFetchHook(options: PageHookOptions): void {
  const pageWindow = window as typeof window & Record<string, unknown>;
  const currentFetch = window.fetch as typeof window.fetch & Record<string, unknown>;
  if (
    pageWindow[options.stateKey] === options.version ||
    currentFetch[options.fetchMarkerKey] === options.version
  ) {
    return;
  }

  const post = (message: unknown): void => {
    window.postMessage(message, options.targetOrigin);
  };

  window.addEventListener('message', (event: MessageEvent<unknown>) => {
    if (event.source !== window || event.origin !== options.targetOrigin) return;
    const data = event.data;
    if (!data || typeof data !== 'object') return;
    const record = data as Record<string, unknown>;
    if (record.type !== options.probeMessage || typeof record.token !== 'string') return;
    if (record.token.length < 8) return;
    post({ type: options.readyMessage, version: options.version, token: record.token });
  });

  const originalFetch = window.fetch.bind(window);
  const maybeCapture = async (
    input: RequestInfo | URL,
    method: string,
    response: Response,
  ): Promise<void> => {
    try {
      let url: string;
      if (typeof input === 'string') url = input;
      else if (input instanceof URL) url = input.href;
      else url = input.url;

      const parsed = new URL(url);
      if (
        parsed.origin !== options.targetOrigin ||
        (method !== 'GET' && method !== 'POST') ||
        !options.chatPathHints.some((hint) => parsed.pathname.includes(hint))
      ) {
        return;
      }

      const text = await response.clone().text();
      const bytes = new TextEncoder().encode(text).byteLength;
      if (bytes > options.maxRawBytes) return;
      post({
        type: options.captureMessage,
        payload: { url, method, status: response.status, text, capturedAt: Date.now() },
      });
    } catch {
      // Capture is best-effort and must never alter page fetch behaviour.
    }
  };

  const hookedFetch: typeof window.fetch = async (input, init) => {
    const response = await originalFetch(input, init);
    let inputMethod = 'GET';
    if (typeof input !== 'string' && 'method' in input && typeof input.method === 'string') {
      inputMethod = input.method;
    }
    const method = String(init?.method ?? inputMethod).toUpperCase();
    void maybeCapture(input, method, response);
    return response;
  };

  Object.defineProperty(hookedFetch, options.fetchMarkerKey, {
    configurable: false,
    enumerable: false,
    value: options.version,
    writable: false,
  });
  window.fetch = hookedFetch;
  pageWindow[options.stateKey] = options.version;
  // Best-effort initial signal; the isolated side also probes with a token so
  // this signal cannot be lost merely because content-script order differs.
  post({ type: options.readyMessage, version: options.version, token: null });
}
