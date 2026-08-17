/**
 * Shared contracts between the three must be one file with no imports,
 * because the MAIN-world script is bundled without extension APIs.
 */

/** Message names used by the page-world hook and the isolated bridge. */
export const CAPTURE_MESSAGE = '__chat_stasher_capture__';
export const MAIN_READY_MESSAGE = '__chat_stasher_main_ready__';
export const MAIN_PROBE_MESSAGE = '__chat_stasher_main_probe__';
export const MAIN_VERIFY_RESULT_MESSAGE = '__chat_stasher_main_verify_result__';
/**
 * Page-world signal for an OBSERVED WebSocket frame. Deliberately a separate
 * name from CAPTURE_MESSAGE: observation is not capture, nothing downstream
 * saves it yet, and the bridge must not mistake one for the other.
 */
export const WS_OBSERVED_MESSAGE = '__chat_stasher_ws_observed__';

/** Shared page-world marker: both injection paths consult the same state. */
export const PAGE_HOOK_VERSION = 'v1';
export const PAGE_HOOK_STATE_KEY = '__chat_stasher_fetch_hook_state__';
export const PAGE_HOOK_FETCH_MARKER = '__chat_stasher_fetch_hook_marker__';

/** Capability wait: short enough to precede normal app traffic, no browser sniffing. */
export const MAIN_FALLBACK_TIMEOUT_MS = 100;

/** Open string: adding a platform must not require a type/logic edit. */
export type PlatformId = string;
export type CaptureConfidence = 'from-source' | 'unverified';

export interface ResponseShape {
  encoding: 'json' | 'text';
  /** Every listed path must be present for JSON responses. */
  requiredPaths?: readonly string[];
  /** At least one listed path must be present for JSON responses. */
  requiredAnyPaths?: readonly string[];
  /** Every listed marker must be present for text responses. */
  requiredTextIncludes?: readonly string[];
}

/** Data-only description of one capturable platform. */
export interface ChatPlatform {
  id: PlatformId;
  /** Exact page/API origins. Never <all_urls>. */
  origins: readonly string[];
  pathHints: readonly string[];
  methods: readonly string[];
  status: { min: number; max: number };
  responseShape: ResponseShape;
  /** Regex source strings; the first capture group is the session id. */
  sessionIdPatterns: readonly string[];
  /** Source-backed is not the same as live verified. */
  credibility: CaptureConfidence;
  /**
   * Opt-in, per row: "this platform carries conversation data over WebSocket".
   * Absent/false (the default for every row shipped today) means the MAIN-world
   * WebSocket wrapper observes NOTHING on that origin. Turning it on is how a
   * future task onboards a WS platform; it is not something to flip casually,
   * because it is the only switch that makes us read frame payloads at all.
   */
  webSocketCapture?: boolean;
}

/**
 * The platform table. Adding support means adding one row of data; the hook,
 * bridge, validator, and saver all consume these generic fields.
 */
export const PLATFORMS: readonly ChatPlatform[] = [
  {
    id: 'deepseek',
    origins: ['https://chat.deepseek.com'],
    pathHints: ['/api/v0/chat', '/chat/session'],
    methods: ['GET', 'POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      requiredAnyPaths: [
        'session_id',
        'sessionId',
        'data.session_id',
        'data.sessionId',
        'data.chat_session_id',
        'data.messages',
        'messages',
      ],
    },
    sessionIdPatterns: [
      '/chat/session/([0-9a-fA-F-]{8,})',
      '[?&]chat_session_id=([^&]+)',
    ],
    // External source evidence checked 2026-08-17 (source code, not README):
    // deepseek-pp (Apache-2.0; commit 0a02c72b135bf2936e11aa78fd6136931ed65908,
    // 2026-08-14) uses https://chat.deepseek.com plus
    // /api/v0/chat/history_messages and /api/v0/chat_session/fetch_page, and
    // requires chat_sessions/chat_messages in the decoded business data:
    // https://github.com/zhu1090093659/deepseek-pp/blob/0a02c72b135bf2936e11aa78fd6136931ed65908/core/deepseek/conversation-export.ts#L105-L186
    // https://github.com/zhu1090093659/deepseek-pp/blob/0a02c72b135bf2936e11aa78fd6136931ed65908/core/export/normalize.ts#L44-L73
    // better-deepseek (MIT; commit f558441ac616a174119ba434571c1ee0a2b84ddb,
    // 2026-08-15) independently uses /api/v0/chat/history_messages, /chat/s/<id>,
    // role/fragments and non-empty content for export:
    // https://github.com/EdgeTypE/better-deepseek/blob/f558441ac616a174119ba434571c1ee0a2b84ddb/src/content/tools/exporter.js#L23-L127
    // Context Sync (MIT; commit 66a548840c1e11f4080e0f059783728173494998,
    // 2026-04-05) independently identifies non-empty DeepSeek DOM message nodes:
    // https://github.com/Vineetpandey0/context-sync/blob/66a548840c1e11f4080e0f059783728173494998/injectors/deepseek.js#L94-L120
    // The external API route/shape differences may represent different entry
    // points or versions; this task changes credibility only, not match data.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'perplexity',
    origins: ['https://www.perplexity.ai'],
    // 🔴 C27 · 只登记会话列表这一条精确路径；正文路径没有出处，不能放宽成前缀。
    pathHints: ['/rest/thread/list_ask_threads'],
    methods: ['POST'],
    status: { min: 200, max: 299 },
    // 回溯枚举器会对列表形状做更严格的顶层数组 + thread_id 检查。
    // 这里的通用 capture gate 只负责不把非 JSON 当作这个平台的流量。
    responseShape: { encoding: 'json' },
    // 列表响应不是正文捕获；本单也没有单条正文 URL 的出处，因此不猜 URL id。
    sessionIdPatterns: [],
    // R26 三源交叉（2026-08-17）；未做真实端到端验证。
    credibility: 'from-source',
    webSocketCapture: false,
  },
  {
    id: 'chatgpt',
    origins: ['https://chatgpt.com', 'https://chat.openai.com'],
    pathHints: ['/backend-api/conversation/'],
    methods: ['GET'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      requiredPaths: ['mapping', 'current_node'],
    },
    sessionIdPatterns: ['/backend-api/conversation/([0-9a-fA-F-]{8,})'],
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'gemini',
    origins: ['https://gemini.google.com'],
    pathHints: ['/_/BardChatUi/data/batchexecute'],
    methods: ['POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'text',
      requiredTextIncludes: ['wrb.fr', 'hNvQHb'],
    },
    sessionIdPatterns: ['/app/([A-Za-z0-9_-]{8,})'],
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'claude',
    origins: ['https://claude.ai'],
    // Deliberately NOT '/api/organizations/' — every org-scoped call (settings,
    // projects, ...) would then become a "conversation data" candidate and the
    // shape-mismatch warning would turn into noise. This hint is the
    // conversation-DETAIL route only. Note it has a trailing slash, so the
    // conversation-LIST route ('/chat_conversations', an array of summaries we
    // do not capture) falls outside and is skipped silently, which is correct:
    // it is not the data we claim to back up.
    // Also deliberately query-free: the two sources below disagree on the
    // casing of the tree flag ('?tree=true' vs '?tree=True').
    pathHints: ['/chat_conversations/'],
    methods: ['GET'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      // Exactly the check the MIT exporter performs before it will export.
      // Required (not "any of"): on this route a body without chat_messages is
      // the drift case, so it must fail the shape gate and get warned about
      // rather than pass through as an empty-looking capture.
      requiredPaths: ['chat_messages'],
    },
    sessionIdPatterns: [
      '/chat_conversations/([0-9a-fA-F-]{8,})',
      '/chat/([0-9a-fA-F-]{8,})',
    ],
    // 🔴 The route shape below is SECOND-HAND: it comes from reading public
    // open-source exporters, NOT from a logged-in claude.ai session. Nobody on
    // this change ever opened claude.ai, so this row is source-backed, never
    // live-verified. If the real route or envelope differs, the generic gate
    // above rejects it and page-hook.ts warns — it never guesses.
    //
    // External source evidence checked 2026-08-17 (source code, not README):
    // claude-chat-exporter (MIT; commit
    // 12da324dd158e9472251590d89d957fc767c0d85, 2026-08-08) requests
    // /api/organizations/<org>/chat_conversations/<uuid>?tree=true&... and
    // validates the response with Array.isArray(data.chat_messages), treating a
    // missing chat_messages as "the endpoint may have changed" rather than as
    // an empty conversation:
    // https://github.com/agarwalvishal/claude-chat-exporter/blob/12da324dd158e9472251590d89d957fc767c0d85/claude-chat-exporter.js#L66
    // https://github.com/agarwalvishal/claude-chat-exporter/blob/12da324dd158e9472251590d89d957fc767c0d85/claude-chat-exporter.js#L449-L452
    // Its CLAUDE.md documents the envelope as { name, model,
    // current_leaf_message_uuid, chat_messages: [{ uuid, parent_message_uuid,
    // index, sender, created_at, content }] }.
    // claude-extension (Apache-2.0; commit
    // 89a20167bd71d0d5700a3679f22b5458c32b7e58, 2026-06-10) independently hooks
    // the same route from a MAIN-world fetch interceptor with
    // /^https:\/\/claude\.ai\/api\/organizations\/[\w-]+\/chat_conversations\/[\w-]+\?tree=True/ :
    // https://github.com/abhimanyu-sikarwar/claude-extension/blob/89a20167bd71d0d5700a3679f22b5458c32b7e58/src/content/inject.js#L5
    // That one matches on URL alone and never inspects the body, so it cannot
    // tell drift from an empty chat — which is exactly why we add the body gate
    // instead of copying its approach.
    // A third project (withLinda/claude-project-conversations-exporter)
    // documents the same GET /api/organizations/[org]/chat_conversations/[conv]
    // but ships NO LICENSE, so it was read for architecture only and no code
    // from it was used.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'kimi',
    // 🔴 DOMAIN: verified against source, not against my own assumptions. The
    // brief suggested 'https://kimi.moonshot.cn'; the CURRENT web app is
    // https://www.kimi.com and that is the only origin here. kimi.moonshot.cn
    // is the LEGACY origin (it still appears in a 2026-02 reverse-API project,
    // paired with a completely different '/api/chat/<id>/...' route family);
    // we have no source-backed evidence that today's page serves the route
    // below from it, so it is deliberately NOT onboarded. Adding it would widen
    // the content-script match set for a guess. One origin, closed set.
    origins: ['https://www.kimi.com'],
    // Deliberately NOT '/apiv2/' or 'ChatService' — every gateway call (send,
    // list, usage, ...) would then become a "conversation data" candidate and
    // the shape-mismatch warning would turn into noise. This hint is the
    // message-LIST-for-one-conversation route only. Written without the leading
    // service package because two sources disagree on it
    // ('kimi.gateway.chat.v1.ChatService' vs 'kimi.chat.v1.ChatService'), and
    // both are the same call. The conversation-INDEX route ('.../ListChats', an
    // array of chat summaries we do not capture) falls outside and is skipped
    // silently, which is correct: it is not the data we claim to back up.
    pathHints: ['ChatService/ListMessages'],
    // Connect-style unary RPC: the request is a POST with a JSON body, and the
    // chat id lives in that body, not in the URL. Hence the page-URL fallback
    // in sessionIdPatterns below.
    methods: ['POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      // Exactly the field the MIT exporter reads before it will export.
      // Required (not "any of"): on this route a body without `messages` is the
      // drift case, so it must fail the shape gate and get warned about rather
      // than pass through as an empty-looking capture.
      requiredPaths: ['messages'],
    },
    // Only the page URL carries the id, so a capture without pageUrl yields no
    // session id and is skipped (logged, not saved) — the existing behaviour.
    sessionIdPatterns: ['/chat/([A-Za-z0-9_-]{8,})'],
    // 🔴 The route shape below is SOURCE-BACKED but NOT live-verified: it comes
    // from reading public open-source projects, NOT from a logged-in Kimi
    // session. Nobody on this change ever opened kimi.com, so this row is
    // 'from-source', never 'verified'. If the real route or envelope differs,
    // the generic gate above rejects it and page-hook.ts warns — it never
    // guesses. Kimi is also known to run front-end signing/WAF challenges; that
    // affects the page's own requests, not us — we only read what the page
    // already fetched.
    //
    // External source evidence checked 2026-08-17 (source code, not README):
    // conreo/kimi-chat-exporter (MIT; commit
    // 9e3956b17ee44bceb453fea2107b9d6263ac0cd6, 2026-06-06) POSTs JSON to
    // https://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages
    // with { chatId }, reads `data.messages`, and treats an absent/empty list
    // as an error rather than as an empty conversation; its own manifest
    // matches only https://www.kimi.com/* and its page menus key off
    // https://www.kimi.com/chat/*:
    // https://github.com/conreo/kimi-chat-exporter/blob/9e3956b17ee44bceb453fea2107b9d6263ac0cd6/background.js#L78-L80
    // https://github.com/conreo/kimi-chat-exporter/blob/9e3956b17ee44bceb453fea2107b9d6263ac0cd6/background.js#L117-L123
    // https://github.com/conreo/kimi-chat-exporter/blob/9e3956b17ee44bceb453fea2107b9d6263ac0cd6/manifest.json#L42-L44
    // AshleyOSLab/kimi-chat-exporter (MIT; commit
    // f27ca71d58eef9012535b2c7708c8865f641946e, 2026-03-15) independently uses
    // BASE_URL https://www.kimi.com and a ListMessages call keyed by chat id,
    // reading `messages` (with `items` / `data.messages` as fallbacks) — note
    // it spells the service 'kimi.chat.v1.ChatService', which is why the path
    // hint above stops at 'ChatService/ListMessages':
    // https://github.com/AshleyOSLab/kimi-chat-exporter/blob/f27ca71d58eef9012535b2c7708c8865f641946e/exporters/kimi_exporter.py#L103-L115
    // springrain1/kimi-pp (Apache-2.0; commit
    // 6edf5494532845102e174d5f22669548309f5d18, 2026-08-02) independently hooks
    // the same '/apiv2/kimi.gateway.chat.v1.ChatService/' gateway on
    // www.kimi.com from a MAIN-world fetch interceptor:
    // https://github.com/springrain1/kimi-pp/blob/6edf5494532845102e174d5f22669548309f5d18/core/kimi/fetch-interceptor.ts#L6
    // chopper1026/kimi2api (MIT; commit
    // 7f046d8627f275432f82788a6547bc905038738c, 2026-05-14) independently
    // hard-codes KIMI_API_BASE = https://www.kimi.com and the same
    // '/apiv2/kimi.gateway.chat.v1.ChatService/' service prefix:
    // https://github.com/chopper1026/kimi2api/blob/7f046d8627f275432f82788a6547bc905038738c/app/config.py#L48
    // https://github.com/chopper1026/kimi2api/blob/7f046d8627f275432f82788a6547bc905038738c/app/kimi/protocol.py#L8
    // xiaoY233/Kimi-Free-API is GPL-3.0, so it was read for architecture only
    // and no code from it was used; it is cited solely for the fact that the
    // LEGACY origin https://kimi.moonshot.cn used an unrelated '/api/chat/...'
    // route family, which is why that origin is not in `origins`.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
];

/** Content-script matches derived from the table — a closed set. */
export const CONTENT_MATCHES: string[] = Array.from(
  new Set(PLATFORMS.flatMap((platform) => platform.origins.map((origin) => `${origin}/*`))),
);

/** Convenience back-compat alias for the incumbent platform origin. */
export const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
export const CHAT_PATH_HINTS = ['/api/v0/chat', '/chat/session'];

/** Look up a platform by exact origin. */
export function getPlatformByOrigin(origin: string): ChatPlatform | undefined {
  return PLATFORMS.find((platform) => platform.origins.includes(origin));
}

/** Sizes above this are streamed media, not session JSON — skip. */
export const MAX_RAW_BYTES = 4 * 1024 * 1024;

export const INBOX_PREFIX = 'chat-stasher/inbox';

export interface CapturedFetch {
  /** Full response URL, e.g. https://chat.deepseek.com/api/v0/chat/... */
  url: string;
  method: string;
  status: number;
  /** Raw response text, passed through untouched so nothing is lost. */
  text: string;
  /** Needed when the provider API URL does not contain the conversation id. */
  pageUrl?: string;
  capturedAt: number;
  /**
   * 🔴 C21 · 会话身份的【权威值】，由「已经知道它是谁」的那一方一路带下来。
   *
   * 根因回顾：同一个会话的身份以前被表达了两次 —— 欠账键是列表接口的
   * items[].id，文件名却是「从 URL 里再抠一次」。两次表达之间隔着有损函数，
   * 于是两个不同的欠账键可以塌成同一个文件名（后写的覆盖先写的）。
   * 这个字段就是那第二次表达的【消除口】：回溯腿把枚举给的 id 直接放进来，
   * 落盘不再推导。
   *
   * 🔴 谁可以填：只有扩展自己（lib/backfill/engine.ts）。
   *    页面来的载荷一律【不许】带这个字段 —— 见 isCapturedFetchShape：
   *    能指定身份就等于能指定写到哪个文件名。
   * 🔴 不填（undefined）⇒ 走 extractSessionId 的老路，实时腿行为逐字不变。
   */
  sessionId?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object';
}

function getJsonPath(value: unknown, path: string): unknown {
  let current: unknown = value;
  for (const part of path.split('.')) {
    if (!current || typeof current !== 'object' || !(part in current)) return undefined;
    current = (current as Record<string, unknown>)[part];
  }
  return current;
}

function hasUsablePath(value: unknown, path: string): boolean {
  const found = getJsonPath(value, path);
  return found !== undefined && found !== null;
}

export function findPlatformForUrl(url: string): ChatPlatform | null {
  try {
    const origin = new URL(url).origin;
    return getPlatformByOrigin(origin) ?? null;
  } catch {
    return null;
  }
}

export function matchesResponseShape(platform: ChatPlatform, text: string): boolean {
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
}

/** Validate page-originated capture payloads before they reach extension APIs. */
export function isCapturedFetchShape(value: unknown): value is CapturedFetch {
  if (!isRecord(value)) return false;
  if (typeof value.url !== 'string' || typeof value.method !== 'string') return false;
  const platform = platformForTraffic(value.url, value.method);
  if (!platform) return false;
  if (
    typeof value.status !== 'number' ||
    !Number.isInteger(value.status) ||
    value.status < platform.status.min ||
    value.status > platform.status.max
  ) return false;
  if (typeof value.text !== 'string' || value.text.length === 0) return false;
  if (value.pageUrl !== undefined && typeof value.pageUrl !== 'string') return false;
  // 🔴 C21：`sessionId` 是扩展内部的权威身份通道（回溯腿用），它直接决定文件名。
  //    页面能填它 = 页面能指定写到哪个文件（覆盖别的会话）。所以这里不是「校验它」，
  //    而是【存在即拒收】—— 一条页面来的载荷根本不该有这个字段。
  if ('sessionId' in value) return false;
  if (typeof value.capturedAt !== 'number' || !Number.isFinite(value.capturedAt) || value.capturedAt <= 0) {
    return false;
  }
  return (
    new TextEncoder().encode(value.text).byteLength <= MAX_RAW_BYTES &&
    matchesResponseShape(platform, value.text)
  );
}

export function isCaptureMessage(
  value: unknown,
): value is { type: typeof CAPTURE_MESSAGE; payload: CapturedFetch } {
  return isRecord(value) && value.type === CAPTURE_MESSAGE && isCapturedFetchShape(value.payload);
}

export function isMainReadyMessage(
  value: unknown,
): value is { type: typeof MAIN_READY_MESSAGE; version: string; token: string } {
  return (
    isRecord(value) &&
    value.type === MAIN_READY_MESSAGE &&
    value.version === PAGE_HOOK_VERSION &&
    typeof value.token === 'string' &&
    value.token.length >= 8
  );
}

export function isMainVerifyResultMessage(
  value: unknown,
): value is { type: typeof MAIN_VERIFY_RESULT_MESSAGE; version: string; token: string; installed: boolean } {
  return (
    isRecord(value) &&
    value.type === MAIN_VERIFY_RESULT_MESSAGE &&
    value.version === PAGE_HOOK_VERSION &&
    typeof value.token === 'string' &&
    value.token.length >= 8 &&
    typeof value.installed === 'boolean'
  );
}

/** v1 kept only so the CLI can recognise legacy bundles; producers write @2. */
export const SCHEMA_V1 = 'chat-stasher/inbox@1';
export const SCHEMA = 'chat-stasher/inbox@2';

/**
 * ADR-002 identity axis: the account, not the machine. Two hosts capturing the
 * same session must produce the same identity value so the CLI can dedupe.
 * Degradation chain (weakest protection for the CLI is 'default'):
 *   platform_uid -> email -> handle -> default
 */
export type IdentityLevel = 'platform_uid' | 'email' | 'handle' | 'default';

export interface InboxIdentity {
  /** Where `value` came from. 'default' means "not found, identity unreliable". */
  level: IdentityLevel;
  /** Stable per-account value. '' when level === 'default'. */
  value: string;
}

export interface InboxBundle {
  schema: typeof SCHEMA;
  platform: PlatformId;
  sessionId: string;
  identity: InboxIdentity;
  url: string;
  method: string;
  status: number;
  capturedAt: string;
  /** Parsed-once envelope fields, best-effort. Raw text is authoritative. */
  parsed: {
    hasJson: boolean;
    keys: string[];
  };
  raw: {
    text: string;
    bytes: number;
  };
}

/** READS raw body. After completion the response body is already consumed; use response.clone(). */
export function isChatTraffic(url: string, method: string): boolean {
  return platformForTraffic(url, method) !== null;
}

export function platformForTraffic(url: string, method: string): ChatPlatform | null {
  try {
    const u = new URL(url);
    return PLATFORMS.find(
      (platform) =>
        platform.origins.includes(u.origin) &&
        platform.methods.includes(method.toUpperCase()) &&
        platform.pathHints.some((hint) => u.pathname.includes(hint)),
    ) ?? null;
  } catch {
    return null;
  }
}

/**
 * Extract a session id from URL or parsed body so the inbox file is stable per session.
 * Returns null when no id can be found — such captures are skipped (logged, not saved).
 */
export function extractSessionId(url: string, text: string, pageUrl?: string): string | null {
  const platform = findPlatformForUrl(url) ?? (pageUrl ? findPlatformForUrl(pageUrl) : null);
  if (platform) {
    for (const pattern of platform.sessionIdPatterns) {
      const match = new RegExp(pattern).exec(url) ?? (pageUrl ? new RegExp(pattern).exec(pageUrl) : null);
      if (match?.[1]) {
        try {
          return decodeURIComponent(match[1]);
        } catch {
          return match[1];
        }
      }
    }
  }
  try {
    const obj = JSON.parse(text);
    if (!obj || typeof obj !== 'object') return null;
    const cand =
      obj.session_id ??
      obj.sessionId ??
      obj.data?.session_id ??
      obj.chat_session?.id ??
      obj.meta?.session_id ??
      null;
    if (typeof cand === 'string' && cand.length >= 8) return cand;
  } catch { /* not JSON — not a capturable session envelope */ }
  return null;
}

/** ADR-002 chain order. Unit tests assert this exact order. */
export const IDENTITY_LEVEL_ORDER: readonly IdentityLevel[] = [
  'platform_uid',
  'email',
  'handle',
  'default',
];

/**
 * Resolve the account-stable identity (ADR-002: account axis, not machine) from
 * a captured chat/session response BODY, then fall down the chain. Reads only
 * `text` — never opens new network calls, never touches storage, so this cannot
 * widen the extension's surface beyond the traffic we already hook.
 *
 * HONESTY: the candidate key names below were NOT verified against a real
 * logged-in DeepSeek page during this spike (no live login allowed), so they are
 * conjectured naming variants, not confirmed wire fields. The structural guards
 * (email regex / digit-long-id / not-session-shaped / not-the-session-id itself)
 * make a wrong guess degrade to `default` instead of emitting a bogus id.
 */
export const IDENTITY_KEY_CANDIDATES: Array<{ level: IdentityLevel; keys: string[] }> = [
  { level: 'platform_uid', keys: ['user_id', 'userId', 'uid', 'author_id', 'owner_id', 'creator_id', 'account_id'] },
  { level: 'email', keys: ['email', 'user_email', 'email_address', 'mail'] },
  { level: 'handle', keys: ['username', 'user_name', 'handle', 'nickname', 'display_name', 'name'] },
];

const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const DASHY_RE = /^[0-9a-fA-F-]+$/;
const IDENTIFIER_RE = /^[A-Za-z0-9_\-:.]{6,}$/;

function isSessionShaped(s: string): boolean {
  // A dashed UUID-ish token is session-shaped; a plain digit run is NOT.
  return s.includes('-') && DASHY_RE.test(s);
}

function acceptIdentityValue(level: IdentityLevel, raw: unknown, sessionId: string | null): string | null {
  if (typeof raw === 'number') raw = String(raw);
  if (typeof raw !== 'string') return null;
  const s = raw.trim();
  if (!s || s === 'null' || s === 'undefined' || s.length > 4096) return null;
  // Never let the per-session id itself impersonate an account id.
  if (sessionId && s === sessionId) return null;
  if (level === 'email') {
    return EMAIL_RE.test(s) ? s : null;
  }
  if (level === 'platform_uid') {
    // Dashed/UUID values are session-shaped, not account-shaped.
    if (isSessionShaped(s)) return null;
    if (/^\d+$/.test(s)) return s.length >= 6 ? s : null;
    return IDENTIFIER_RE.test(s) ? s : null;
  }
  // handle: a human-ish name, not an email, not a bare number.
  if (s.length > 64 || EMAIL_RE.test(s) || /^\d+$/.test(s)) return null;
  return s;
}

/** Bounded walk (depth <= 3) over JSON bodies looking for the candidate keys. */
function collectFirst(node: unknown, keys: ReadonlyArray<string>, out: unknown[], depth: number): void {
  if (depth > 3 || out.length) return;
  if (Array.isArray(node)) {
    for (const item of node) collectFirst(item, keys, out, depth + 1);
    return;
  }
  if (!node || typeof node !== 'object') return;
  for (const [key, val] of Object.entries(node as Record<string, unknown>)) {
    if (!keys.includes(key)) {
      collectFirst(val, keys, out, depth + 1);
    } else if (typeof val === 'string' || typeof val === 'number') {
      out.push(val);
    }
  }
}

export function extractIdentity(text: string, sessionId: string | null = null): InboxIdentity {
  let body: unknown = null;
  try {
    body = JSON.parse(text);
  } catch { /* not JSON → no account fields to read → default */ }
  if (!body || typeof body !== 'object') return { level: 'default', value: '' };
  for (const group of IDENTITY_KEY_CANDIDATES) {
    const found: unknown[] = [];
    collectFirst(body, group.keys, found, 0);
    for (const raw of found) {
      const accepted = acceptIdentityValue(group.level, raw, sessionId);
      if (accepted !== null) return { level: group.level, value: accepted };
    }
  }
  return { level: 'default', value: '' };
}

export function sanitizePathSegment(s: string): string {
  return s.replace(/[\x00-\x1f/\\:*?"<>| ]/g, '_');
}

/**
 * 🔴 C21 · 会话 id → 文件名片段，**必须是单射的**（两个不同的 id 绝不可能得到同一个片段）。
 *
 * 为什么不是「把不安全字符换成 _ 就完事」：`sanitizePathSegment` 是**多对一**的
 * （'a b' 和 'a/b' 都变成 'a_b'），而 lib/download.ts 是 conflictAction:'overwrite' ——
 * 两个不同的会话塌成同一个名字，后写的会把先写的**从磁盘上抹掉**。
 *
 * 这里选的是【恒等 + 拒收】而不是【转义】：
 *  · 片段安全（sanitize 是空操作）⇒ 原样返回。在这个定义域上命名函数就是恒等映射，
 *    单射是**结构性**的，不依赖任何字符表的细节。
 *  · 片段不安全 ⇒ 返回 null，调用方必须当成一次【留痕的失败】，绝不硬塞一个名字。
 *
 * 🔴 为什么不用转义（例如 '_'→'_5f'、' '→'_20'）：那样会把**已有用户**所有
 *    含 '_' 的会话（gemini / kimi 的 id 字符集是 [A-Za-z0-9_-]）重命名一遍，
 *    等于把整个收件箱重下一遍。恒等方案对所有既有的、路径安全的 id
 *    **一个字节都不变** —— 实时腿的既有行为因此可以逐字保持。
 */
export function pathSafeSessionId(id: string): string | null {
  if (!id) return null;
  // 恒等即单射：只接受「原样就能当文件名」的 id。
  if (sanitizePathSegment(id) !== id) return null;
  // '.'/'..' 会被当成路径而不是名字；'.' 开头也不该出现在收件箱里。
  if (id.startsWith('.')) return null;
  return id;
}
