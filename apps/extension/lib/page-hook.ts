import {
  CAPTURE_MESSAGE,
  MAX_RAW_BYTES,
  MAIN_PROBE_MESSAGE,
  MAIN_READY_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_STATE_KEY,
  PAGE_HOOK_VERSION,
  PLATFORMS,
  WS_OBSERVED_MESSAGE,
  type ChatPlatform,
} from './contract';

/** Fixed, metadata-only signal for a supported-origin transport we do not capture. */
export const UNSUPPORTED_TRANSPORT_WARNING =
  '[chat-stasher] unsupported transport candidate detected; capture not attempted';

/**
 * Fixed, metadata-only signal for the failure mode this whole capability exists
 * to make visible: an origin whose platform row SAYS it speaks WebSocket, where
 * our wrapper did not end up installed (CSP, a frozen global, the page grabbing
 * the constructor first, or no WebSocket at all). Without this line that case is
 * completely silent — the page just works and we back up nothing.
 */
export const WEBSOCKET_HOOK_UNINSTALLED_WARNING =
  '[chat-stasher] websocket hook not installed on a websocket-declared origin';

/** Values are serialized into the fallback <script>; no extension API belongs here. */
export interface PageHookOptions {
  captureMessage: string;
  probeMessage: string;
  readyMessage: string;
  stateKey: string;
  fetchMarkerKey: string;
  /** Page message name for an observed WebSocket frame (not a capture). */
  wsObservedMessage: string;
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
  wsObservedMessage: WS_OBSERVED_MESSAGE,
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
  const unsupportedTransportWarning =
    '[chat-stasher] unsupported transport candidate detected; capture not attempted';
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

  const baseUrl = typeof pageWindow.location.href === 'string'
    ? pageWindow.location.href
    : `${pageOrigin}/`;
  const warnedUnsupportedTransports = new Set<string>();
  const getCandidatePlatform = (url: string): ChatPlatform | null => {
    try {
      const parsed = new URL(url, baseUrl);
      const candidateOrigin = parsed.protocol === 'wss:'
        ? `https://${parsed.host}`
        : parsed.protocol === 'ws:'
          ? `http://${parsed.host}`
          : parsed.origin;
      const platform = options.platforms.find((item) => item.origins.includes(candidateOrigin));
      const pageTransportOrigin = pageOrigin;
      const samePageOrigin = candidateOrigin === pageTransportOrigin;
      if (!platform || !samePageOrigin || !platform.pathHints.some((hint) => parsed.pathname.includes(hint))) {
        return null;
      }
      return platform;
    } catch {
      return null;
    }
  };
  const warnUnsupportedTransport = (transport: string, url: string): void => {
    const platform = getCandidatePlatform(url);
    if (!platform) return;
    const key = `${platform.id}:${transport}`;
    if (warnedUnsupportedTransports.has(key)) return;
    warnedUnsupportedTransports.add(key);
    console.warn(unsupportedTransportWarning);
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

  const xhrConstructor = pageWindow.XMLHttpRequest;
  if (typeof xhrConstructor === 'function') {
    const originalOpen = xhrConstructor.prototype.open;
    const originalSend = xhrConstructor.prototype.send;
    const xhrUrls = new WeakMap<object, string>();

    xhrConstructor.prototype.open = function (
      this: XMLHttpRequest,
      method: string,
      url: string | URL,
      ...rest: unknown[]
    ): void {
      xhrUrls.set(this, String(url));
      originalOpen.apply(this, [method, url, ...rest] as never);
    };
    xhrConstructor.prototype.send = function (
      this: XMLHttpRequest,
      body?: Document | XMLHttpRequestBodyInit | null,
    ): void {
      const url = xhrUrls.get(this);
      if (url) {
        this.addEventListener('load', () => {
          if (this.status >= 200 && this.status < 300) warnUnsupportedTransport('xhr', url);
        }, { once: true });
      }
      originalSend.call(this, body);
    };
  }

  const eventSourceConstructor = pageWindow.EventSource;
  if (typeof eventSourceConstructor === 'function') {
    pageWindow.EventSource = new Proxy(eventSourceConstructor, {
      construct(target, args, newTarget) {
        const source = Reflect.construct(target, args, newTarget) as EventSource;
        const url = String(args[0]);
        const warn = () => warnUnsupportedTransport('sse', url);
        source.addEventListener('open', warn, { once: true });
        source.addEventListener('message', warn, { once: true });
        return source;
      },
    });
  }

  // ---- WebSocket ----------------------------------------------------------
  // Opt-in per platform row and OFF everywhere else: an origin only gets its
  // frames looked at when its own row says webSocketCapture. Everything below
  // is observation only — we never send a frame, never replace onmessage/send,
  // and never keep the page from seeing its own events.
  const wsUninstalledWarning =
    '[chat-stasher] websocket hook not installed on a websocket-declared origin';
  const wsPlatform =
    options.platforms.find(
      (platform) => platform.origins.includes(pageOrigin) && platform.webSocketCapture === true,
    ) ?? null;

  /** Returns true when the frame was observed, so the caller skips the "unsupported" warn. */
  const observeWebSocketFrame = (url: string, data: unknown): boolean => {
    if (!wsPlatform) return false;
    try {
      const parsed = new URL(url, baseUrl);
      const candidateOrigin = parsed.protocol === 'wss:'
        ? `https://${parsed.host}`
        : parsed.protocol === 'ws:'
          ? `http://${parsed.host}`
          : parsed.origin;
      if (candidateOrigin !== pageOrigin || !wsPlatform.origins.includes(candidateOrigin)) return false;
      if (!wsPlatform.pathHints.some((hint) => parsed.pathname.includes(hint))) return false;
      // Binary frames are not decoded here: guessing an encoding would be the
      // "hooked it but parsed it wrong" failure this capability is meant to avoid.
      if (typeof data !== 'string' || data.length === 0) return false;
      if (new TextEncoder().encode(data).byteLength > options.maxRawBytes) return false;
      post({
        type: options.wsObservedMessage,
        payload: {
          platformId: wsPlatform.id,
          url: parsed.href,
          text: data,
          pageUrl: typeof pageWindow.location.href === 'string' ? pageWindow.location.href : undefined,
          observedAt: Date.now(),
        },
      });
      return true;
    } catch {
      return false;
    }
  };

  let webSocketHookInstalled = false;
  const webSocketConstructor = pageWindow.WebSocket;
  // Capability detection, never version sniffing: if any piece we need is
  // missing we leave the page's constructor exactly as we found it.
  if (
    typeof webSocketConstructor === 'function' &&
    typeof Proxy === 'function' &&
    typeof Reflect === 'object' &&
    typeof Reflect.construct === 'function'
  ) {
    try {
      const wrappedWebSocket = new Proxy(webSocketConstructor, {
        construct(target, args, newTarget) {
          const socket = Reflect.construct(target, args, newTarget) as WebSocket;
          const url = String(args[0]);
          try {
            // A plain extra listener: additive, so the page's own listeners and
            // onmessage/onerror/onclose handlers all still run unchanged.
            socket.addEventListener('message', (event: MessageEvent) => {
              try {
                if (!observeWebSocketFrame(url, event?.data)) {
                  warnUnsupportedTransport('websocket', url);
                }
              } catch {
                // Observation must never surface as a page-visible error.
              }
            });
          } catch {
            // A socket that refuses listeners is still returned untouched.
          }
          return socket;
        },
      });
      // The assignment itself can throw (frozen/read-only global). Catching it
      // is the difference between "we quietly did not install" and "we threw an
      // exception into the page at document_start".
      pageWindow.WebSocket = wrappedWebSocket;
      webSocketHookInstalled = pageWindow.WebSocket === wrappedWebSocket;
    } catch {
      webSocketHookInstalled = false;
    }
  }
  if (wsPlatform && !webSocketHookInstalled) {
    // The whole point of task C15: a declared-but-unhooked origin is never silent.
    console.warn(wsUninstalledWarning);
  }

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
