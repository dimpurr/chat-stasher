/**
 * W128 step 1 · **The irreversible account fingerprint every captured bundle carries.**
 *
 * Why this exists: one browser profile can be signed in to account B after having
 * been signed in to account A. Nothing today distinguishes the two inside a
 * bundle, so A's scope can end up holding B's conversation, or B's body can be
 * accepted as A's. The smallest step that makes that visible is to stamp each
 * bundle with a value that is **stable per account and reversible by nobody** —
 * including us, including whoever reads the archive.
 *
 * The shape of the guarantee, stated as an intent rather than a list of attacks:
 *  · **What makes a bundle's fingerprint meaningful** is that the same account on
 *    the same install hashes the same string every time, and a different account
 *    hashes a different one. If either half fails the field is worse than absent,
 *    because it would be read as evidence about an account.
 *  · **What makes it irreversible** is that the input id is never stored, and the
 *    hash is keyed with a random 32-byte secret that exists only in this install's
 *    `storage.local`. So the archive alone cannot recover the id — not by reading
 *    the bundle, not by brute force over a small id space, which a bare hash would
 *    allow.
 *  · **What invalidates it** is the salt: a fingerprint is only comparable to
 *    another fingerprint carrying the same `saltId`. Two installs, two profiles,
 *    or one install whose `cs_account_salt_v1` was cleared produce incomparable
 *    values. This is inherent to "salt stored locally per install" and is why
 *    `saltId` travels with the value instead of being left implicit.
 *  · **Never the raw id, never an email.** The id itself is not written anywhere
 *    by this module. An email is not an account *id*, so when an email is the only
 *    thing a capture offered, the fingerprint is `unknown` with its own reason —
 *    see `AccountUnknownReason`.
 *
 * 🔴 **When there is no id, this says so.** `unknown` is a value on the bundle with
 *    a named reason, never an absent field and never a placeholder hash. A
 *    fingerprint computed from a guessed id would be indistinguishable from a real
 *    one at every later step, which is exactly the failure this project refuses
 *    (CLAUDE.md invariant 1). Nothing in this module ever falls back to hashing
 *    something else.
 *
 * 🔴 **Where the salt lives, and where it must never go.** `storage.local` only,
 *    under `cs_account_salt_v1` — never `storage.sync` (`docs/privacy.md`), and
 *    never into a page. All hashing happens in the extension's service worker, so
 *    the secret never crosses into a content script or the MAIN world; what travels
 *    from a page is the id, which the capture path already reads.
 */
import { extractIdentity, findPlatformForUrl, type AccountFingerprint, type AccountIdSource, type AccountUnknownReason, type CapturedFetch } from './contract';
import { backfillPlanFor } from './backfill/enumerate';
import { orgFromRequestUrl } from './backfill/claude-org';
import type { BackfillStore } from './backfill/store';

/** The one key this module owns. Never synced: `storage.local` only. */
export const ACCOUNT_SALT_KEY = 'cs_account_salt_v1';

/** 32 bytes = the HMAC-SHA256 key length. */
export const ACCOUNT_SALT_BYTES = 32;

/**
 * Domain separation, **including the platform id**.
 *
 * The message is not the bare id: two platforms could issue the same id string,
 * and a fingerprint that did not say which platform it was over would then be
 * mistaken for the same account. The prefix is also a version tag, so a future
 * algorithm cannot silently produce a value that looks like this one's.
 */
export const ACCOUNT_FINGERPRINT_DOMAIN = 'chat-stasher/account-fingerprint/v1';

/** What is stored under `ACCOUNT_SALT_KEY`. */
interface StoredAccountSalt {
  /** The public half: an opaque per-install id. Not the key, and not derived from it. */
  id: string;
  /** The secret half, base64 of `ACCOUNT_SALT_BYTES` random bytes. */
  key: string;
  /** When this install's salt was created, ms since epoch. */
  createdAt: number;
}

export interface AccountSalt {
  id: string;
  key: Uint8Array;
}

/** The id/unknown decision, before any hashing happens. Exported for tests and for the source table below. */
export type AccountIdReading =
  | { kind: 'id'; id: string; source: AccountIdSource }
  | { kind: 'unknown'; reason: AccountUnknownReason };

function base64FromBytes(bytes: Uint8Array): string {
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary);
}

function bytesFromBase64(text: string): Uint8Array | null {
  try {
    const binary = atob(text);
    const out = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

/**
 * WebCrypto availability. Two separate capabilities are needed and both are
 * checked: HMAC (importKey + sign) for the fingerprint, and `getRandomValues`
 * for the salt. A context with one and not the other cannot do this, and the
 * reason codes below keep them apart so a trace says which one was missing.
 */
function hmacSubtle(): SubtleCrypto | null {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  return c && typeof c.subtle?.importKey === 'function' && typeof c.subtle?.sign === 'function'
    ? c.subtle
    : null;
}

function randomCrypto(): Crypto | null {
  const c = (globalThis as { crypto?: Crypto }).crypto;
  return c && typeof c.getRandomValues === 'function' && typeof c.randomUUID === 'function' ? c : null;
}

/**
 * `key` must be exactly `ACCOUNT_SALT_BYTES` of base64, and `id` a plausible opaque
 * id. Anything else means the stored record is not one this module wrote or is not
 * one it can read.
 *
 * 🔴 **A record that fails this is not replaced by a fresh salt.** Silent
 *    regeneration would change every fingerprint from that moment on while the old
 *    ones stayed in the archive, so two bundles from one unchanged account would
 *    carry different values and be read as an account switch — a false accusation
 *    invented by a repair. The caller gets `salt-unreadable` and the human gets a
 *    visible reason; deleting the key is the deliberate way to start over.
 */
function readSalt(raw: unknown): AccountSalt | { unreadable: true } | null {
  if (raw === null || raw === undefined) return null;
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return { unreadable: true };
  const record = raw as Partial<StoredAccountSalt>;
  const id = record.id;
  const key = record.key;
  if (typeof id !== 'string' || id.length === 0 || id.length > 128) return { unreadable: true };
  if (typeof key !== 'string') return { unreadable: true };
  const bytes = bytesFromBase64(key);
  if (bytes === null || bytes.length !== ACCOUNT_SALT_BYTES) return { unreadable: true };
  return { id, key: bytes };
}

/**
 * The install's salt, created on first use.
 *
 * Three outcomes, and the third is not the second: a salt, "there is none yet"
 * (which is created here), and "there is one and it cannot be read". A store that
 * is absent is a fourth: this returns null and the caller reports
 * `salt-unavailable` rather than inventing a salt that would not survive a reload.
 */
export async function loadOrCreateAccountSalt(
  store: BackfillStore | null,
): Promise<AccountSalt | 'unreadable' | null> {
  if (!store) return null;
  let existing: unknown = null;
  try {
    existing = await store.load(ACCOUNT_SALT_KEY);
  } catch {
    // A store that cannot be read is not a store that is empty. `unreadable` is
    // the honest answer: the caller must not create a salt over the top of one
    // that may be there, because that would silently re-key every later bundle.
    return 'unreadable';
  }
  const parsed = readSalt(existing);
  if (parsed === null) {
    const c = randomCrypto();
    if (!c) return null;
    const key = new Uint8Array(ACCOUNT_SALT_BYTES);
    c.getRandomValues(key);
    const created: StoredAccountSalt = {
      id: c.randomUUID(),
      key: base64FromBytes(key),
      createdAt: Date.now(),
    };
    try {
      await store.save(ACCOUNT_SALT_KEY, created);
    } catch {
      // The salt was not persisted ⇒ its fingerprints would not be reproducible
      // after a reload, so there is nothing to return. The id is not a salt.
      return null;
    }
    return { id: created.id, key };
  }
  if ('unreadable' in parsed) return 'unreadable';
  return parsed;
}

/**
 * HMAC-SHA256 of `domain \0 platform \0 id`, lowercase hex.
 * Returns null when WebCrypto HMAC is unavailable — the caller must then report
 * `crypto-unavailable`, never a bare digest and never the id.
 */
export async function fingerprintAccountId(
  salt: AccountSalt,
  domain: string,
  platform: string,
  id: string,
): Promise<string | null> {
  const subtle = hmacSubtle();
  if (!subtle) return null;
  try {
    const key = await subtle.importKey(
      'raw',
      salt.key as unknown as BufferSource,
      { name: 'HMAC', hash: 'SHA-256' },
      false,
      ['sign'],
    );
    const message = `${domain}\u0000${platform}\u0000${id}`;
    const signature = await subtle.sign('HMAC', key, new TextEncoder().encode(message));
    let out = '';
    for (const b of new Uint8Array(signature)) out += b.toString(16).padStart(2, '0');
    return out;
  } catch {
    return null;
  }
}

/**
 * The account id a capture makes visible, or the named fact that it makes none.
 * Pure: no storage, no network, no clock.
 *
 * 🔴 W128 · **Where the id comes from, per platform.** Two mechanisms, and the
 *    difference between them is the whole point of recording the source on the
 *    bundle rather than only the hash:
 *
 *  · **Claude — `request-url-organization`, verified, page-owned, and already
 *    load-bearing.** claude.ai addresses every conversation by organization and
 *    puts the id in the path of the page's own request, which is why the condition
 *    below is `backfillPlanFor(id).scopeInPath` rather than the string `'claude'`:
 *    the day a second platform addresses its conversations the same way, it is
 *    covered without an edit here. The id is read through `orgFromRequestUrl`,
 *    whose predicate accepts only the endpoint's uuid shape, so a conversation
 *    title sitting in that field cannot be read as an organization (claude-org.ts's
 *    W49 note).
 *
 *  · **ChatGPT, Gemini, Grok, DeepSeek, Perplexity — `response-body-platform-uid`,
 *    the ADR-002 account axis, labelled as exactly that.** These five expose no
 *    account id in any path we see (they are captured on conversation-detail routes
 *    only), so the only account-shaped value visible is the one the ledger already
 *    scopes by: `extractIdentity`'s `platform_uid` level. It is used here **because
 *    it is the same value the run scope is built from**, so the fingerprint cannot
 *    disagree with the scope it exists to protect. It is *not* presented as a
 *    verified platform field: contract.ts's own note says those key names were
 *    never confirmed against a logged-in page, and a wrong match degrades to
 *    another level rather than to a bogus id (`acceptIdentityValue`). ChatGPT is
 *    the one of the five whose stable id is known to exist — the `ChatGPT-Account-Id`
 *    request header (ADR-031) — and it is exactly the one this build cannot see:
 *    no request header is captured (`CapturedFetch` carries none), so ChatGPT
 *    reports `unknown` here rather than being special-cased on a hope.
 *
 *  · **Email and handle are refused as inputs.** `extractIdentity` will report an
 *    email, and an email does identify an account — but it is not an account/org
 *    *id*, and it is the exact value the instruction names as forbidden. So an
 *    email-only or handle-only reading is `unknown`, with a reason code that says
 *    which one it was. This is the conservative reading of an ambiguous
 *    instruction; see the report's open-question note.
 *
 * `sessionId` is passed in rather than re-derived so the session-id guard below
 * uses the same value the bundle is named by (background.ts's `resolveSessionId`,
 * C21). A value that IS the session id is not an account id.
 */
export function accountIdFromCapture(captured: CapturedFetch, sessionId: string | null): AccountIdReading {
  const row = findPlatformForUrl(captured.url)
    ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  if (!row) return { kind: 'unknown', reason: 'platform-not-recognized' };

  if (backfillPlanFor(row.id)?.scopeInPath) {
    const org = orgFromRequestUrl(captured.url);
    return org === null
      ? { kind: 'unknown', reason: 'organization-not-in-request-url' }
      : { kind: 'id', id: org, source: 'request-url-organization' };
  }

  const identity = extractIdentity(captured.text, sessionId);
  switch (identity.level) {
    case 'platform_uid':
      return { kind: 'id', id: identity.value, source: 'response-body-platform-uid' };
    case 'email':
      return { kind: 'unknown', reason: 'email-is-not-an-account-id' };
    case 'handle':
      return { kind: 'unknown', reason: 'handle-is-not-an-account-id' };
    case 'default':
      return { kind: 'unknown', reason: 'no-account-id-in-capture' };
  }
}

/**
 * The bundle's `account` field. Always answers with one of the two kinds — there is
 * no third "not applicable", because every bundle either carries a fingerprint or
 * carries the fact that it does not.
 *
 * Never throws: a capture whose fingerprint cannot be computed is still a capture,
 * and losing the conversation over a metadata step would be the worst trade here.
 */
export async function accountFingerprintFor(
  captured: CapturedFetch,
  store: BackfillStore | null,
  sessionId: string | null,
): Promise<AccountFingerprint> {
  const row = findPlatformForUrl(captured.url)
    ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  const reading = accountIdFromCapture(captured, sessionId);
  if (reading.kind === 'unknown') return { kind: 'unknown', reason: reading.reason };
  if (!row) return { kind: 'unknown', reason: 'platform-not-recognized' };

  if (!hmacSubtle()) return { kind: 'unknown', reason: 'crypto-unavailable' };
  const salt = await loadOrCreateAccountSalt(store);
  if (salt === 'unreadable') return { kind: 'unknown', reason: 'salt-unreadable' };
  if (salt === null) return { kind: 'unknown', reason: 'salt-unavailable' };

  const value = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, row.id, reading.id);
  if (value === null) return { kind: 'unknown', reason: 'crypto-unavailable' };
  return { kind: 'fingerprint', value, source: reading.source, saltId: salt.id };
}
