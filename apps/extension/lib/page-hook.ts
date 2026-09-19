import {
  CAPTURE_MESSAGE,
  GEMINI_AT_KEY,
  GEMINI_BL_KEY,
  GEMINI_ORIGIN,
  GEMINI_SESSION_ID_KEY,
  GEMINI_TOKENS_REPLY_MESSAGE,
  GEMINI_TOKENS_REQUEST_MESSAGE,
  GEMINI_WIZ_GLOBAL_DATA_KEY,
  HOOK_REASON_DID_NOT_TAKE,
  HOOK_REASON_WAS_REPLACED,
  HOOK_REPORT_MESSAGE,
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
import { CHATGPT_PAGED_DETAIL_PATTERN, CONVERSATION_SEEN_MESSAGE } from './platform-auth';

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
  /** Page message carrying only a conversation id whose paged window the page loaded. */
  conversationSeenMessage: string;
  /**
   * 🔴 W43 · Page message name for "this hook could not install a patch, or a
   * patch it installed is no longer in effect" (see `lib/hook-status.ts`).
   *
   * It travels in the options rather than being imported by the function body for
   * the same reason every other name here does: the fallback path serialises this
   * object and calls `installPageFetchHook.toString()`, so an identifier the
   * function body closes over would be undefined in the injected copy.
   */
  hookReportMessage: string;
  /** 🔴 W43 · The two reason codes this hook may report, held as data (closed set: `lib/contract.ts`). */
  hookReportReasons: {
    /** A patch did not take effect: the page's global would not accept it. */
    didNotTake: string;
    /** The patch was in effect and something replaced it afterwards. */
    wasReplaced: string;
  };
  /** RegExp source (serialisable) for ChatGPT's paged detail path; group 1 = id. */
  chatgptPagedDetailPattern: string;
  /**
   * 🔴 W29 · The Gemini bootstrap-token pull (see GEMINI_TOKENS_*_MESSAGE in
   * lib/contract.ts). Every value here is a **name**, not a secret: the hook reads
   * the tokens out of the page's own blob at the moment it is asked, and holds
   * them for the length of one synchronous reply.
   */
  geminiTokensRequestMessage: string;
  geminiTokensReplyMessage: string;
  /**
   * The page's bootstrap blob and the three values inside it. `origin` is the one
   * page this hook will answer for — the platform row that needs these, written
   * down as data rather than re-derived.
   */
  geminiBootstrap: {
    globalKey: string;
    atKey: string;
    blKey: string;
    fSidKey: string;
    origin: string;
  };
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
  conversationSeenMessage: CONVERSATION_SEEN_MESSAGE,
  hookReportMessage: HOOK_REPORT_MESSAGE,
  hookReportReasons: {
    didNotTake: HOOK_REASON_DID_NOT_TAKE,
    wasReplaced: HOOK_REASON_WAS_REPLACED,
  },
  chatgptPagedDetailPattern: CHATGPT_PAGED_DETAIL_PATTERN.source,
  geminiTokensRequestMessage: GEMINI_TOKENS_REQUEST_MESSAGE,
  geminiTokensReplyMessage: GEMINI_TOKENS_REPLY_MESSAGE,
  // 🔴 The blob's key names and the three slots this hook may read out of it are
  //    page facts, kept beside the message names rather than inside the function
  //    body: the fallback path serialises this object, so anything the hook needs
  //    at runtime has to travel in it. The origin is the closed set of one — the
  //    only page whose globals this hook will read.
  geminiBootstrap: {
    globalKey: GEMINI_WIZ_GLOBAL_DATA_KEY,
    atKey: GEMINI_AT_KEY,
    blKey: GEMINI_BL_KEY,
    fSidKey: GEMINI_SESSION_ID_KEY,
    origin: GEMINI_ORIGIN,
  },
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

  /**
   * 🔴 W43 · **The origin of this document's own realm, not of its URL.**
   *
   * `location.origin` answers a question about the *URL*, and for `about:blank`
   * and `about:srcdoc` that answer is the string `"null"` — even though such a
   * document **inherits its parent's origin** and the browser treats its requests
   * as same-origin with that parent. Measured 2026-09-19 in this repository's own
   * Chromium, in a same-origin subframe of a matched origin: inside it
   * `location.origin === 'null'` while `window.origin === 'https://chatgpt.com'`,
   * and a `fetch` from it came back **200 with a readable body** — which a
   * genuinely cross-origin response could not do without CORS headers.
   *
   * 🔴 Using `location.origin` here produced two real failures in that frame, and
   *    they are one mistake: a URL-less same-origin document was treated as a
   *    foreign, opaque one.
   *      · `post(message, 'null')` **throws** `SyntaxError` in Chromium — so a
   *        hook installed in such a frame could not post a capture, could not
   *        answer a probe, and could not report its own failure. The isolated
   *        bridge in the same frame is a separate script with the same line, so
   *        the two could not have met even if one of them had spoken.
   *      · `parsed.origin !== pageOrigin` was true for every request such a frame
   *        made, so a conversation fetched from it was refused by our own gate.
   *
   * `window.origin` is the environment settings object's origin, which is what
   * both checks are actually asking. For a page served over http(s) the two are
   * the same string, so this changes nothing there.
   *
   * 🔴 A genuinely opaque document (a sandboxed frame with no origin of its own)
   *    reports `"null"` from `window.origin` **too**, and every boundary below
   *    stays exactly as it was for it: no platform lists `"null"` among its
   *    origins, so nothing is captured and nothing is posted. The fallback to
   *    `location.origin` covers a realm with no `origin` at all (the node test
   *    stubs), where the two agree by construction.
   */
  const pageOrigin = ((): string => {
    const environmentOrigin = typeof pageWindow.origin === 'string' ? pageWindow.origin : '';
    return environmentOrigin.length > 0 ? environmentOrigin : pageWindow.location.origin;
  })();
  const getPlatform = (url: string): ChatPlatform | null => {
    try {
      const origin = new URL(url).origin;
      return options.platforms.find((platform) => platform.origins.includes(origin)) ?? null;
    } catch {
      return null;
    }
  };

  /**
   * 🔴 W43 · **What a relative URL in this document resolves against.**
   *
   * `location.href` answers that question for a page served over http(s), and it
   * is the wrong answer for a URL-less document: `new URL('/api', 'about:blank')`
   * **throws**, so on an `about:blank` subframe every relative request the page
   * made reached this hook and died in the catch below it — the frame's traffic
   * was invisible even with the hook correctly installed in it and an origin that
   * matched. The environment origin is what those requests actually resolve
   * against, so it is what they resolve against here.
   *
   * A document with neither (a realm whose `location` has nothing usable) keeps
   * the old behaviour, and a genuinely opaque one produces `"null/"`, which the
   * same catch turns back into "not a candidate" rather than a wrong capture.
   */
  const baseUrl = ((): string => {
    const href = pageWindow.location.href;
    if (typeof href !== 'string') return `${pageOrigin}/`;
    return href.startsWith('http:') || href.startsWith('https:') ? href : `${pageOrigin}/`;
  })();
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

  // 🔴 W28 · Own properties only — `'entries' in []` is true, because an array
  // inherits `Array.prototype.entries`. See the same walker in lib/contract.ts:
  // the two copies must agree, because this one decides whether a payload is
  // posted and that one decides whether the bridge accepts it.
  const getJsonPath = (value: unknown, path: string): unknown => {
    let current: unknown = value;
    for (const part of path.split('.')) {
      if (!current || typeof current !== 'object') return undefined;
      if (!Object.prototype.hasOwnProperty.call(current, part)) return undefined;
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

  /**
   * 🔴 W43 · **Posting never throws, for the same reason capturing never does.**
   *
   * `postMessage` refuses a `targetOrigin` that is not a valid origin by
   * **throwing** `SyntaxError` (measured in Chromium), and one document shape
   * produces exactly that value: a genuinely opaque frame, whose environment
   * origin serialises as the string `"null"`. There the install-time readiness
   * signal was an exception thrown into the page at `document_start`, leaving a
   * half-installed hook — the one outcome this function is written to avoid.
   *
   * Nothing is lost by swallowing it: a document with no origin of its own can
   * never match a platform row, so no message from it could have been acted on.
   * The failure that this hides is already recorded where it matters — the bridge
   * in that frame reports its own inability to verify the hook, and background
   * drops that report because `"null"` is not one of the eight origins
   * (`entrypoints/background.ts`).
   */
  const post = (message: unknown): void => {
    try {
      window.postMessage(message, pageOrigin);
    } catch {
      // Never into the page. See above for why nothing is missed.
    }
  };

  /**
   * 🔴 W43 · **The hook's own observation of itself, sent to the bridge.**
   *
   * This is the one thing this function may say about a failure, and it is what
   * turns "the page was not captured" from a silence into a recorded fact
   * (`lib/hook-status.ts`). Two rules keep it honest:
   *
   *  · It is only ever called with an observation this function actually made —
   *    a patch whose read-back disagrees, or a wrapper that was gone when the
   *    page asked. It is never called because capture "seems" not to be working:
   *    a page with no traffic is not a broken page.
   *  · It is best-effort in exactly the way capture is. A page that refuses
   *    `postMessage` must not get an exception thrown into it from here, which is
   *    the same rule the rest of this function follows.
   */
  const reportHookFailure = (reason: string): void => {
    try {
      post({ type: options.hookReportMessage, reason });
    } catch {
      // Never into the page. The failure is already visible in the DOM mutation
      // this function could not perform; this line is the extension's record.
    }
  };

  /**
   * The one capture decision, shared by fetch and XHR: platform, origin,
   * method, path, status, size and response shape. A request that is not a
   * candidate is ignored; a candidate whose body does not match gets the
   * metadata-only shape warning. Never throws into the page.
   */
  const captureCandidate = (rawUrl: string, method: string, status: number, text: string): void => {
    try {
      const parsed = new URL(rawUrl, baseUrl);
      const platform = getPlatform(parsed.href);
      const normalizedMethod = method.toUpperCase();
      if (
        !platform ||
        parsed.origin !== pageOrigin ||
        !platform.methods.includes(normalizedMethod) ||
        !platform.pathHints.some((hint) => parsed.pathname.includes(hint)) ||
        status < platform.status.min ||
        status > platform.status.max
      ) return;

      const bytes = new TextEncoder().encode(text).byteLength;
      if (bytes > options.maxRawBytes) {
        // A conversation too large to carry is still a conversation: say so.
        // Metadata only — never the URL, body, or identifiers.
        console.warn('[chat-stasher] capture skipped: response exceeds the size cap');
        return;
      }
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
          status,
          text,
          pageUrl: typeof pageWindow.location.href === 'string' ? pageWindow.location.href : undefined,
          capturedAt: Date.now(),
        },
      });
    } catch {
      // Capture is best-effort and must never alter page behaviour.
    }
  };

  // ---- XMLHttpRequest -----------------------------------------------------
  // Some platforms load conversations over XHR rather than fetch (DeepSeek's
  // /api/v0/chat/history_messages, observed in a live session 2026-09-13).
  // A readable body goes through the same capture decision as fetch. Only
  // bodies that cannot be read without changing what the page sees
  // (arraybuffer / blob / document) stay a visible "unsupported" warning.
  const xhrConstructor = pageWindow.XMLHttpRequest;
  /** The two signatures this hook replaces on `XMLHttpRequest.prototype`. */
  type XhrOpenWrapper = (
    this: XMLHttpRequest,
    method: string,
    url: string | URL,
    ...rest: unknown[]
  ) => void;
  type XhrSendWrapper = (
    this: XMLHttpRequest,
    body?: Document | XMLHttpRequestBodyInit | null,
  ) => void;
  /**
   * 🔴 W43 · **Which transports this hook actually patched.**
   *
   * Read back rather than assumed, and that is the whole point: a plain
   * assignment to a read-only global is *silent* in the sloppy-mode function this
   * bundle compiles to, so "I assigned it" and "the page now uses my function"
   * are two different facts and only the second one is capture. Before W43 the
   * hook reported neither, so a page that refused a patch looked exactly like a
   * page where nothing had been requested yet.
   *
   * 🔴 W43b · **And the read-back is of the live global, every time it is asked.**
   *    The first version compared `xhrConstructor.prototype.open` — the object
   *    this function captured a line earlier — against the function it had before
   *    the assignment. That answers "did my assignment take?" and nothing else. A
   *    page that replaces `window.XMLHttpRequest` *afterwards* leaves the captured
   *    object exactly as we left it, so on that page the read-back said "ours"
   *    while the constructor the page actually calls had a native `open` again —
   *    the measured half-install (fetch wrapped, `XMLHttpRequest.prototype.open`
   *    native, on a logged-in page whose capture goes through XHR). What is
   *    compared now is `pageWindow.XMLHttpRequest.prototype.*`: the object the
   *    page's own code will reach for, re-read on every question rather than
   *    remembered from install time.
   */
  let xhrPatched = false;
  /**
   * 🔴 W43b · The wrappers this function installs, kept so the live global can be
   *    compared against **them** rather than against "whatever is there now" — a
   *    page that replaced `open` with its own function is not a page we patched.
   *    `null` until the block below runs, and for a document with no XHR global.
   */
  let xhrOpenWrapper: XhrOpenWrapper | null = null;
  let xhrSendWrapper: XhrSendWrapper | null = null;
  /**
   * Whether this document had an XHR transport to hook at all. A page with no
   * `XMLHttpRequest` global has no XHR capture to lose, so its absence is
   * "nothing to install" rather than a missing half — the opposite reading would
   * file a failure against every document that never had the transport.
   */
  const xhrHookExpected = typeof xhrConstructor === 'function';

  /**
   * 🔴 W43b · **Is the XHR half of this installer in effect, right now?**
   *
   * The one question both the install below and the probe listener ask, so the
   * two cannot drift: `true` only while the live `window.XMLHttpRequest` is a
   * constructor whose `prototype.open` **and** `prototype.send` are the functions
   * this hook installed. `send` is part of the answer because it is the half that
   * attaches the capture listener — a constructor that kept our `open` and
   * recovered a native `send` records the request and then reads nothing.
   *
   * A global that is missing, unreadable, or not constructible is not a wrapper
   * this hook installed, which is the answer that keeps a refusal from reading as
   * a success.
   */
  const xhrHookInEffect = (): boolean => {
    if (!xhrHookExpected) return true;
    if (!xhrOpenWrapper || !xhrSendWrapper) return false;
    try {
      const live = pageWindow.XMLHttpRequest;
      if (typeof live !== 'function') return false;
      return live.prototype?.open === xhrOpenWrapper && live.prototype?.send === xhrSendWrapper;
    } catch {
      return false;
    }
  };

  if (xhrHookExpected) {
    const originalOpen = xhrConstructor.prototype.open;
    const originalSend = xhrConstructor.prototype.send;
    const xhrRequests = new WeakMap<object, { url: string; method: string }>();

    const openWrapper: XhrOpenWrapper = function (
      this: XMLHttpRequest,
      method: string,
      url: string | URL,
      ...rest: unknown[]
    ): void {
      xhrRequests.set(this, { url: String(url), method: String(method ?? 'GET') });
      originalOpen.apply(this, [method, url, ...rest] as never);
    };
    const sendWrapper: XhrSendWrapper = function (
      this: XMLHttpRequest,
      body?: Document | XMLHttpRequestBodyInit | null,
    ): void {
      const request = xhrRequests.get(this);
      if (request) {
        this.addEventListener('load', () => {
          try {
            if (this.status < 200 || this.status >= 300) return;
            const type = this.responseType;
            let text: string | null = null;
            if (type === '' || type === 'text') {
              text = this.responseText;
            } else if (type === 'json' && this.response !== null && this.response !== undefined) {
              // Re-serialised, so not byte-identical to the wire; the structure is.
              text = JSON.stringify(this.response);
            }
            if (text === null) {
              warnUnsupportedTransport('xhr', request.url);
              return;
            }
            captureCandidate(request.url, request.method, this.status, text);
          } catch {
            // Never let observation break the page's own request.
          }
        }, { once: true });
      }
      originalSend.call(this, body);
    };
    xhrOpenWrapper = openWrapper;
    xhrSendWrapper = sendWrapper;
    // 🔴 W43b · Guarded exactly as the fetch assignment below and the WebSocket
    //    patch above are, and for the same reason: a prototype whose setter throws
    //    (a frozen `XMLHttpRequest.prototype`, measured in `w12-csp-handshake`) is
    //    a **recorded** observation, not an exception thrown into the page at
    //    `document_start` that abandons the rest of this installer — which is what
    //    it used to do, leaving the fetch half unwrapped as a side effect of an
    //    XHR refusal. The read-back below names it; there is nothing to add here.
    try {
      xhrConstructor.prototype.open = openWrapper;
      xhrConstructor.prototype.send = sendWrapper;
    } catch {
      // See above: the read-back is the report.
    }
    // The read-back — of the live global, not of the object captured above. An
    // assignment the page's own descriptor refused leaves this false, and so does
    // a page that has since replaced the constructor; both are facts about this
    // page worth recording.
    xhrPatched = xhrHookInEffect();
  }

  const eventSourceConstructor = pageWindow.EventSource;
  if (typeof eventSourceConstructor === 'function') {
    const eventSourceProxy = new Proxy(eventSourceConstructor, {
      construct(target, args, newTarget) {
        const source = Reflect.construct(target, args, newTarget) as EventSource;
        const url = String(args[0]);
        const warn = () => warnUnsupportedTransport('sse', url);
        source.addEventListener('open', warn, { once: true });
        source.addEventListener('message', warn, { once: true });
        return source;
      },
    });
    pageWindow.EventSource = eventSourceProxy;
    // 🔴 W43 · Read back, for the same reason the fetch patch is read back: a
    //    refused assignment is silent in this bundle, and an EventSource the page
    //    kept is a transport we would otherwise claim to have covered.
    if (pageWindow.EventSource !== eventSourceProxy) {
      reportHookFailure(options.hookReportReasons.didNotTake);
    }
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

      // ChatGPT's in-page navigation loads only a paged window of the
      // conversation (no `mapping`). Archiving that window would store a
      // partial conversation, so it is never captured; instead the id alone is
      // handed to the isolated side, which fetches the full conversation.
      const paged = platform && platform.id === 'chatgpt' && parsed.origin === pageOrigin
        && normalizedMethod === 'GET' && response.status >= 200 && response.status <= 299
        ? new RegExp(options.chatgptPagedDetailPattern).exec(parsed.pathname)
        : null;
      if (paged) {
        post({ type: options.conversationSeenMessage, platform: 'chatgpt', id: paged[1] });
        return;
      }

      if (
        !platform ||
        parsed.origin !== pageOrigin ||
        !platform.methods.includes(normalizedMethod) ||
        !platform.pathHints.some((hint) => parsed.pathname.includes(hint)) ||
        response.status < platform.status.min ||
        response.status > platform.status.max
      ) return;

      // Only candidates are cloned and read; the decision itself is shared with XHR.
      const text = await response.clone().text();
      captureCandidate(parsed.href, normalizedMethod, response.status, text);
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
  try {
    window.fetch = hookedFetch;
  } catch {
    // 🔴 W43 · A global whose setter throws is the same fact as one that swallows
    //    the assignment, and neither may become an exception thrown into the page
    //    at `document_start`. The read-back below is what reports it, so the two
    //    shapes end in the same recorded observation. The WebSocket patch has
    //    guarded its assignment this way since it was written.
  }

  /**
   * 🔴 W43 · **Was the patch taken? Read the global back and see.**
   *
   * This is not paranoia and it is not a re-check of our own arithmetic. The
   * assignment above is a plain one, and this whole bundle compiles to a
   * sloppy-mode IIFE (measured in the built `content-scripts/dw-fetch-main.js`:
   * no `"use strict"`), so against a read-only or accessor-guarded `window.fetch`
   * the assignment **does not throw and does not take** — the page keeps its own
   * function and every capture path in this file goes dead while the hook still
   * reports itself alive and answers probes. That is the one shape where the old
   * code's silence was the *hook's own* fault rather than the page's, and
   * `window.fetch === hookedFetch` is the cheapest possible way to see it.
   *
   * 🔴 A `false` here is a **recorded failure, not a thrown one**: the report goes
   *    to the bridge (`lib/hook-status.ts`), the probe listener is not registered
   *    (so the isolated side's handshake cannot mistake this page for a captured
   *    one), and the state marker is not written (so a later injection — the
   *    fallback, or a re-inject from a fresh document — is free to try again).
   *    Nothing is thrown into the page: a page whose globals are read-only is
   *    doing something deliberate, and it must not be broken by our reaction to
   *    it.
   */
  const fetchPatched = window.fetch === hookedFetch;
  if (!fetchPatched) {
    reportHookFailure(options.hookReportReasons.didNotTake);
    return;
  }
  if (xhrHookExpected && !xhrPatched) {
    // 🔴 W43b · The XHR requests this page makes are invisible, and that is a fact
    //    its row must carry rather than a detail. Capture over `fetch` still works,
    //    so the wrapper above stays installed — but the hook is **not** whole, and
    //    the probe listener below refuses to answer from this state. Answering was
    //    the second half of the old bug: one healthy wrapper was enough to say
    //    "captured", and that answer is what withdraws a record (a page whose hook
    //    is half-installed had its own record removed moments after it appeared).
    reportHookFailure(options.hookReportReasons.didNotTake);
  }

  // 🔴 The probe listener is registered **here**, after the wrapper is actually
  //    in place, and not a line earlier. Answering a probe is the isolated
  //    side's proof that this hook is installed (it is what replaced the inline
  //    verifier DeepSeek's CSP refuses to execute), so "answered" has to imply
  //    "installed" by construction rather than by luck.
  //    🔴 W43 · And "installed" is now read back rather than assumed: the answer
  //    below is sent only while the wrappers this function installed are still
  //    the page's own globals, so a page that replaced one after the patch — the
  //    one way a page can silently end capture mid-life — gets a recorded
  //    observation instead of a reassuring answer. See the verification inside
  //    the listener.
  //    🔴 W43b · **Both** halves, re-read on every probe. The fetch-only check was
  //    exactly the hole: a page whose `window.fetch` is ours and whose
  //    `XMLHttpRequest.prototype.open` is native again (measured, on a page whose
  //    capture goes through XHR) answered this probe, and that answer withdrew a
  //    record that had been written correctly a moment earlier. "It was ours when
  //    we installed it" is not "it is ours now", and the only moment the isolated
  //    side asks is now.
  //    Still atomic with respect to the probe: `window.postMessage` is delivered
  //    as a task, so no probe can be dispatched in the middle of this function.
  window.addEventListener('message', (event: MessageEvent<unknown>) => {
    if (event.source !== window || event.origin !== pageOrigin) return;
    const data = event.data;
    if (!data || typeof data !== 'object') return;
    const record = data as Record<string, unknown>;
    if (record.type !== options.probeMessage || typeof record.token !== 'string') return;
    if (record.token.length < 8) return;
    if (window.fetch !== hookedFetch) {
      // 🔴 W43 · The wrapper was in effect when it was installed and is not any
      //    more: something on this page replaced `window.fetch`. Reported, not
      //    answered — answering would tell the bridge "this page is captured"
      //    while the page's own calls go straight past us, which is precisely
      //    the false negative this handshake exists to remove.
      reportHookFailure(options.hookReportReasons.wasReplaced);
      return;
    }
    if (!xhrHookInEffect()) {
      // 🔴 W43b · The measured half-install, and the reason the answer above may
      //    not be sent from it. `fetch` being ours says nothing about the XHR
      //    global the page now holds, and on this shape the page's own capture
      //    goes through XHR — so an answer here would be the false negative this
      //    handshake exists to remove, and worse than a false negative: `null`
      //    from the bridge **withdraws** the origin's record.
      //    The reason names which of the two shapes it is, and both are
      //    observations: `did-not-take` if the patch never took at install time,
      //    `was-replaced` if it took and the page has since put its own function
      //    back.
      reportHookFailure(
        xhrPatched ? options.hookReportReasons.wasReplaced : options.hookReportReasons.didNotTake,
      );
      return;
    }
    post({ type: options.readyMessage, version: options.version, token: record.token });
  });

  /**
   * 🔴 W29 · **The Gemini bootstrap-token pull** (see lib/contract.ts).
   *
   * A content script cannot see a page global, so this hook is the only part of
   * this extension that can read `WIZ_global_data` — and it reads it **when
   * asked**, not at install time, so a page that rotates the value between two
   * requests is followed rather than remembered. Nothing is cached here: the
   * values exist as locals for the length of one synchronous reply.
   *
   * 🔴 Answering is limited to the one origin whose own row needs these values,
   *    and the reply carries exactly the three slots named in the options — no
   *    wider read of the blob, and nothing that is not one of those three keys.
   *    What the page itself can already read is unchanged by this; the argument
   *    for the channel is written out in lib/contract.ts beside the message
   *    names, and it is the reason there is no secret to protect here.
   */
  const bootstrap = options.geminiBootstrap;
  const readBootstrapTokens = (): Record<string, string | null> => {
    const empty = { at: null, bl: null, fSid: null };
    try {
      const blob = pageWindow[bootstrap.globalKey];
      if (!blob || typeof blob !== 'object') return empty;
      const read = (key: string): string | null => {
        const value = (blob as Record<string, unknown>)[key];
        return typeof value === 'string' && value.length > 0 ? value : null;
      };
      return { at: read(bootstrap.atKey), bl: read(bootstrap.blKey), fSid: read(bootstrap.fSidKey) };
    } catch {
      // A frozen or unreadable global is "no token", never an exception into the
      // page: the request then goes out without one and the platform's own
      // refusal is what the caller sees.
      return empty;
    }
  };

  window.addEventListener('message', (event: MessageEvent<unknown>) => {
    if (event.source !== window || event.origin !== pageOrigin) return;
    if (pageOrigin !== bootstrap.origin) return;
    const data = event.data;
    if (!data || typeof data !== 'object') return;
    const record = data as Record<string, unknown>;
    if (record.type !== options.geminiTokensRequestMessage) return;
    post({ type: options.geminiTokensReplyMessage, ...readBootstrapTokens() });
  });

  pageWindow[options.stateKey] = options.version;
  // Best-effort initial signal; the isolated side also probes with a token so
  // this signal cannot be lost merely because content-script order differs.
  post({ type: options.readyMessage, version: options.version, token: null });
}
