import {
  CAPTURE_MESSAGE,
  MAX_RAW_BYTES,
  MAIN_PROBE_MESSAGE,
  MAIN_READY_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_STATE_KEY,
  PAGE_HOOK_VERSION,
  PLATFORMS,
  type ChatPlatform,
} from './contract';

/** Values are serialized into the fallback <script>; no extension API belongs here. */
export interface PageHookOptions {
  captureMessage: string;
  probeMessage: string;
  readyMessage: string;
  stateKey: string;
  fetchMarkerKey: string;
  version: string;
  /** Platform table; the hook matches against the CURRENT page's own origin. */
  platforms: ChatPlatform[];
  maxRawBytes: number;
}

export const PAGE_HOOK_OPTIONS: PageHookOptions = {
  captureMessage: CAPTURE_MESSAGE,
  probeMessage: MAIN_PROBE_MESSAGE,
  readyMessage: MAIN_READY_MESSAGE,
  stateKey: PAGE_HOOK_STATE_KEY,
  fetchMarkerKey: PAGE_HOOK_FETCH_MARKER,
  version: PAGE_HOOK_VERSION,
  platforms: PLATFORMS.map((platform) => ({
    ...platform,
    origins: [...platform.origins],
    pathHints: [...platform.pathHints],
    methods: [...platform.methods],
    sessionIdPatterns: [...platform.sessionIdPatterns],
    responseShape: {
      ...platform.responseShape,
      requiredPaths: platform.responseShape.requiredPaths ? [...platform.responseShape.requiredPaths] : undefined,
      requiredAnyPaths: platform.responseShape.requiredAnyPaths ? [...platform.responseShape.requiredAnyPaths] : undefined,
      requiredTextIncludes: platform.responseShape.requiredTextIncludes
        ? [...platform.responseShape.requiredTextIncludes]
        : undefined,
    },
  })),
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

  const pageOrigin = pageWindow.location.origin;
  const getPlatform = (url: string): ChatPlatform | null => {
    try {
      const origin = new URL(url).origin;
      return options.platforms.find((platform) => platform.origins.includes(origin)) ?? null;
    } catch {
      return null;
    }
  };

  const getJsonPath = (value: unknown, path: string): unknown => {
    let current: unknown = value;
    for (const part of path.split('.')) {
      if (!current || typeof current !== 'object' || !(part in current)) return undefined;
      current = (current as Record<string, unknown>)[part];
    }
    return current;
  };

  const hasUsablePath = (value: unknown, path: string): boolean => {
    const found = getJsonPath(value, path);
    return found !== undefined && found !== null;
  };

  const matchesShape = (platform: ChatPlatform, text: string): boolean => {
    const shape = platform.responseShape;
    if (shape.encoding === 'text') {
      return (shape.requiredTextIncludes ?? []).every((marker) => text.includes(marker));
    }
    let body: unknown;
    try {
      body = JSON.parse(text);
    } catch {
      return false;
    }
    const requiredPaths = shape.requiredPaths ?? [];
    const requiredAnyPaths = shape.requiredAnyPaths ?? [];
    return (
      requiredPaths.every((path) => hasUsablePath(body, path)) &&
      (requiredAnyPaths.length === 0 || requiredAnyPaths.some((path) => hasUsablePath(body, path)))
    );
  };

  const post = (message: unknown): void => {
    window.postMessage(message, pageOrigin);
  };

  window.addEventListener('message', (event: MessageEvent<unknown>) => {
    if (event.source !== window || event.origin !== pageOrigin) return;
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

      const baseUrl = typeof pageWindow.location.href === 'string'
        ? pageWindow.location.href
        : `${pageOrigin}/`;
      const parsed = new URL(url, baseUrl);
      const platform = getPlatform(parsed.href);
      const normalizedMethod = method.toUpperCase();
      if (
        !platform ||
        parsed.origin !== pageOrigin ||
        !platform.methods.includes(normalizedMethod) ||
        !platform.pathHints.some((hint) => parsed.pathname.includes(hint)) ||
        response.status < platform.status.min ||
        response.status > platform.status.max
      ) return;

      const text = await response.clone().text();
      const bytes = new TextEncoder().encode(text).byteLength;
      if (bytes > options.maxRawBytes) return;
      if (!matchesShape(platform, text)) {
        // Keep the signal metadata-only: never print URL, body, or identifiers.
        console.warn('[chat-stasher] capture skipped: response shape mismatch');
        return;
      }
      post({
        type: options.captureMessage,
        payload: {
          url: parsed.href,
          method: normalizedMethod,
          status: response.status,
          text,
          pageUrl: typeof pageWindow.location.href === 'string' ? pageWindow.location.href : undefined,
          capturedAt: Date.now(),
        },
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
