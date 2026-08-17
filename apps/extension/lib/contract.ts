/**
 * Shared contracts between the three must be one file with no imports,
 * because the MAIN-world script is bundled without extension APIs.
 */

/** Message names used by the page-world hook and the isolated bridge. */
export const CAPTURE_MESSAGE = '__chat_stasher_capture__';
export const MAIN_READY_MESSAGE = '__chat_stasher_main_ready__';
export const MAIN_PROBE_MESSAGE = '__chat_stasher_main_probe__';
export const MAIN_VERIFY_RESULT_MESSAGE = '__chat_stasher_main_verify_result__';

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
    credibility: 'unverified',
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
