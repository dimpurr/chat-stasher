/**
 * Shared contracts between the three must be one file with no imports,
 * because the MAIN-world script is bundled without extension APIs.
 */

/** CustomEvent name used by the MAIN-world fetch hook to talk to the ISOLATED bridge. */
export const CAPTURE_EVENT = '__chat_stasher_captured__';

/** Only DeepSeek web chat. Deliberately not <all_urls>. */
export const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';

/** Relative prefixes of chat/session traffic worth capturing. */
export const CHAT_PATH_HINTS = ['/api/v0/chat', '/chat/session'];

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
  capturedAt: number;
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
  platform: 'deepseek';
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
  try {
    const u = new URL(url);
    if (u.origin !== DEEPSEEK_ORIGIN) return false;
    if (!(method === 'POST' || method === 'GET')) return false;
    return CHAT_PATH_HINTS.some((hint) => u.pathname.includes(hint));
  } catch {
    return false;
  }
}

/**
 * Extract a session id from URL or parsed body so the inbox file is stable per session.
 * Returns null when no id can be found — such captures are skipped (logged, not saved).
 */
export function extractSessionId(url: string, text: string): string | null {
  try {
    const m = /\/chat\/session\/([0-9a-fA-F-]{8,})/.exec(url);
    if (m && m[1]) return m[1] as string;
  } catch { /* ignore */ }
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