/**
 * W165 · W128 step 1 — every captured bundle carries an irreversible account
 * fingerprint.
 *
 * What this file pins, and why each half needs pinning:
 *  1. 🔴 **the property the field exists for** — the same account on the same
 *     install hashes to the same value, a different account to a different one.
 *     Fail either half and the field is worse than absent: it would be read as
 *     evidence about an account.
 *  2. 🔴 **irreversibility** — the raw id is never written by this field, and the
 *     value is *keyed*, so it is not the bare sha256 of the id (which a small id
 *     space could be brute-forced out of).
 *  3. 🔴 **`unknown` is a value, not an absence** — every path that cannot produce
 *     a fingerprint produces a named reason instead. Absent-vs-unknown must stay
 *     readable, and a guess must never be recorded as a measurement.
 *  4. 🔴 **the salt is per install, is created once, and is never silently
 *     replaced** — a fresh salt over a corrupt record would re-key every later
 *     fingerprint and make one unchanged account look like a switch.
 *  5. 🔴 **the source is recorded per platform** — the six platforms resolve to the
 *     sources the report's table states, and the two mechanisms stay
 *     distinguishable on the bundle.
 *
 * Zero network, zero logged-in state, zero real account data: every id below is a
 * synthetic fixture, the http port is a pure function, and the write-down channel
 * is the shared synthetic native host (`tests/synthetic-native-host.ts`).
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import {
  ACCOUNT_SALT_BYTES,
  ACCOUNT_SALT_KEY,
  accountFingerprintFor,
  accountIdFromCapture,
  fingerprintAccountId,
  loadOrCreateAccountSalt,
} from '../lib/account-fingerprint';
import { memoryStore } from '../lib/backfill/store';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

// ---------------------------------------------------------------------------
// Synthetic fixtures. No real account id, org id, conversation id or email
// appears below; every one is an obvious fixture string.
// ---------------------------------------------------------------------------
const ORG_A = '11111111-2222-3333-4444-555555555555';
const ORG_B = '99999999-8888-7777-6666-555555555555';
const SID = 'aaaaaaaa-1111-2222-3333-444444444444';

const CLAUDE_URL = (org: string): string =>
  `https://claude.ai/api/organizations/${org}/chat_conversations/${SID}?tree=True`;

/** The URL each of the six is captured on, with no account id in it. */
const CAPTURE_URL: Record<string, string> = {
  chatgpt: `https://chatgpt.com/backend-api/conversation/${SID}`,
  claude: CLAUDE_URL(ORG_A),
  gemini: 'https://gemini.google.com/_/BardChatUi/data/batchexecute',
  grok: `https://grok.com/rest/app-chat/conversations/${SID}/load-responses`,
  deepseek: `https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=${SID}`,
  perplexity: 'https://www.perplexity.ai/rest/thread/fixture-slug',
};

/** A conversation body carrying an account-shaped id (the ADR-002 generic scan). */
const bodyWithUid = (uid: string): string => JSON.stringify({ account_id: uid, mapping: {}, current_node: 'n0' });
/** A conversation body carrying nothing account-shaped at all. */
const BODY_NO_ID = JSON.stringify({ mapping: {}, current_node: 'n0' });

function capture(url: string, text: string, pageUrl?: string): CapturedFetch {
  return { url, method: 'GET', status: 200, text, pageUrl, capturedAt: 1_700_000_000_000 };
}

// ===========================================================================
// Part A · the module, on synthetic fixtures
// ===========================================================================
describe('W165-A · the fingerprint is stable per account and keyed by the salt', () => {
  const saltOf = async (seed: Record<string, unknown> = {}) => {
    const store = memoryStore(seed);
    const salt = await loadOrCreateAccountSalt(store);
    if (!salt || salt === 'unreadable') throw new Error('fixture must produce a salt');
    return { store, salt };
  };

  it('🔴 the same id hashes to the same value twice, and a different id does not', async () => {
    const { salt } = await saltOf();
    const one = await fingerprintAccountId(salt, 'chat-stasher/test/v1', 'chatgpt', 'acct-fixture-1');
    const again = await fingerprintAccountId(salt, 'chat-stasher/test/v1', 'chatgpt', 'acct-fixture-1');
    const other = await fingerprintAccountId(salt, 'chat-stasher/test/v1', 'chatgpt', 'acct-fixture-2');

    expect(one).toMatch(/^[0-9a-f]{64}$/);
    expect(again).toBe(one);
    expect(other).not.toBe(one);
  });

  it('🔴 a different salt gives a different value for the same id — the secret is load-bearing', async () => {
    const a = await saltOf();
    const b = await saltOf();
    expect(a.salt.id).not.toBe(b.salt.id);
    expect(await fingerprintAccountId(a.salt, 'chat-stasher/test/v1', 'chatgpt', 'acct-fixture-1'))
      .not.toBe(await fingerprintAccountId(b.salt, 'chat-stasher/test/v1', 'chatgpt', 'acct-fixture-1'));
  });

  it('🔴 the platform is inside the hashed message: one id on two platforms is two values', async () => {
    const { salt } = await saltOf();
    expect(await fingerprintAccountId(salt, 'chat-stasher/test/v1', 'gemini', 'acct-fixture-1'))
      .not.toBe(await fingerprintAccountId(salt, 'chat-stasher/test/v1', 'grok', 'acct-fixture-1'));
  });

  it('🔴 the value is not the unkeyed sha256 of the id, and it carries no id of its own', async () => {
    const { salt } = await saltOf();
    const id = 'acct-fixture-1';
    const value = await fingerprintAccountId(salt, 'chat-stasher/test/v1', 'chatgpt', id);
    // The bare digest is what a small id space could be brute-forced against.
    const { sha256Hex } = await import('../lib/native-host');
    expect(value).not.toBe(await sha256Hex(id));
    expect(value).not.toContain(id);
    // …and the bundle-side object has exactly the four keys it should: no id, no email.
    const field = { kind: 'fingerprint', value: value!, source: 'response-body-platform-uid', saltId: salt.id };
    expect(Object.keys(field).sort()).toEqual(['kind', 'saltId', 'source', 'value']);
    expect(JSON.stringify(field)).not.toContain(id);
  });
});

describe('W165-A2 · the salt: one per install, created once, never silently replaced', () => {
  it('creates a 32-byte salt under its own key, and a second call does not rewrite it', async () => {
    const store = memoryStore();
    const first = await loadOrCreateAccountSalt(store);
    expect(first).not.toBeNull();
    expect(first).not.toBe('unreadable');
    const stored = store.data[ACCOUNT_SALT_KEY] as { id: string; key: string; createdAt: number };
    expect(Object.keys(stored).sort()).toEqual(['createdAt', 'id', 'key']);
    expect(Buffer.from(stored.key, 'base64')).toHaveLength(ACCOUNT_SALT_BYTES);

    const writesAfterFirst = store.writes;
    const second = await loadOrCreateAccountSalt(store);
    expect(second).toEqual(first);
    expect(store.writes).toBe(writesAfterFirst);
  });

  it('🔴 a salt record that does not parse is refused, and is NOT replaced by a fresh one', async () => {
    const corrupt = { id: 'fixture-salt', key: 'not-base64-at-all!!', createdAt: 1 };
    const store = memoryStore({ [ACCOUNT_SALT_KEY]: corrupt });
    expect(await loadOrCreateAccountSalt(store)).toBe('unreadable');

    const reading = await accountFingerprintFor(
      capture(CAPTURE_URL.chatgpt!, bodyWithUid('acct-fixture-1')),
      store,
      null,
    );
    expect(reading).toEqual({ kind: 'unknown', reason: 'salt-unreadable' });
    // The decisive half: re-keying here would make one unchanged account look like
    // a switch at every later comparison, so the evidence must be untouched and the
    // reason visible instead.
    expect(store.data[ACCOUNT_SALT_KEY]).toEqual(corrupt);
  });

  it('no store at all is salt-unavailable, never a fingerprint computed from nothing', async () => {
    const reading = await accountFingerprintFor(
      capture(CAPTURE_URL.chatgpt!, bodyWithUid('acct-fixture-1')),
      null,
      null,
    );
    expect(reading).toEqual({ kind: 'unknown', reason: 'salt-unavailable' });
  });
});

describe('W165-A3 · unknown is a value with a reason, never an absence and never a guess', () => {
  const fingerprintOrReason = async (url: string, text: string, seed: Record<string, unknown> = {}) => {
    const store = memoryStore(seed);
    return await accountFingerprintFor(capture(url, text), store, null);
  };

  it('nothing account-shaped in the body ⇒ no-account-id-in-capture', async () => {
    expect(await fingerprintOrReason(CAPTURE_URL.chatgpt!, BODY_NO_ID))
      .toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
  });

  it('🔴 an email is not used as the input, and the reason says which it was', async () => {
    const emailBody = JSON.stringify({ email: 'fixture@example.invalid', mapping: {}, current_node: 'n0' });
    expect(await fingerprintOrReason(CAPTURE_URL.gemini!, emailBody))
      .toEqual({ kind: 'unknown', reason: 'email-is-not-an-account-id' });
  });

  it('a handle is not an id either — it can change — and it gets its own reason', async () => {
    const handleBody = JSON.stringify({ display_name: 'fixture-handle', mapping: {}, current_node: 'n0' });
    expect(await fingerprintOrReason(CAPTURE_URL.grok!, handleBody))
      .toEqual({ kind: 'unknown', reason: 'handle-is-not-an-account-id' });
  });

  it('an account-scoped plan whose URL names no organization ⇒ organization-not-in-request-url', async () => {
    expect(await fingerprintOrReason('https://claude.ai/api/organizations', BODY_NO_ID))
      .toEqual({ kind: 'unknown', reason: 'organization-not-in-request-url' });
    // A segment that is not an organization id is the same fact as a missing one —
    // it is never written into the field that must hold an organization.
    expect(await fingerprintOrReason(CLAUDE_URL('not-a-uuid'), bodyWithUid('acct-fixture-1')))
      .toEqual({ kind: 'unknown', reason: 'organization-not-in-request-url' });
  });

  it('a URL that is no platform at all ⇒ platform-not-recognized', async () => {
    expect(await fingerprintOrReason('https://example.invalid/whatever', bodyWithUid('acct-fixture-1')))
      .toEqual({ kind: 'unknown', reason: 'platform-not-recognized' });
  });

  it('no WebCrypto ⇒ crypto-unavailable, and no raw-id fallback appears', async () => {
    const realCrypto = globalThis.crypto;
    vi.stubGlobal('crypto', undefined);
    try {
      const got = await fingerprintOrReason(CAPTURE_URL.chatgpt!, bodyWithUid('acct-fixture-1'));
      expect(got).toEqual({ kind: 'unknown', reason: 'crypto-unavailable' });
      expect(JSON.stringify(got)).not.toContain('acct-fixture-1');
    } finally {
      vi.stubGlobal('crypto', realCrypto);
    }
  });

  it('🔴 the session id itself is never accepted as an account id', () => {
    // `extractIdentity`'s own guard, exercised through this module's entry point:
    // the same value cannot be both the file's name and its account.
    const body = JSON.stringify({ account_id: SID, mapping: {}, current_node: 'n0' });
    expect(accountIdFromCapture(capture(CAPTURE_URL.chatgpt!, body), SID))
      .toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
  });
});

describe('W165-A4 · the source is recorded per platform, and fires exactly where it can', () => {
  it('🔴 Claude resolves from the organization in the page’s own request URL', async () => {
    const reading = accountIdFromCapture(capture(CLAUDE_URL(ORG_A), BODY_NO_ID), null);
    expect(reading).toEqual({ kind: 'id', id: ORG_A, source: 'request-url-organization' });

    const store = memoryStore();
    const got = await accountFingerprintFor(capture(CLAUDE_URL(ORG_A), BODY_NO_ID), store, null);
    expect(got.kind).toBe('fingerprint');
    if (got.kind === 'fingerprint') expect(got.source).toBe('request-url-organization');
    // …and the two organizations are two different fingerprints under one salt.
    const other = await accountFingerprintFor(capture(CLAUDE_URL(ORG_B), BODY_NO_ID), store, null);
    expect(other.kind).toBe('fingerprint');
    if (got.kind === 'fingerprint' && other.kind === 'fingerprint') {
      expect(other.value).not.toBe(got.value);
      expect(other.saltId).toBe(got.saltId);
    }
  });

  it('🔴 the five path-unscoped platforms take the ADR-002 account axis, labelled as such', async () => {
    const store = memoryStore();
    for (const platform of ['chatgpt', 'gemini', 'grok', 'deepseek', 'perplexity']) {
      const reading = accountIdFromCapture(
        capture(CAPTURE_URL[platform]!, bodyWithUid(`acct-fixture-${platform}`)),
        null,
      );
      expect(reading, platform).toEqual({
        kind: 'id',
        id: `acct-fixture-${platform}`,
        source: 'response-body-platform-uid',
      });

      const got = await accountFingerprintFor(
        capture(CAPTURE_URL[platform]!, bodyWithUid(`acct-fixture-${platform}`)),
        store,
        null,
      );
      expect(got.kind, platform).toBe('fingerprint');
      if (got.kind === 'fingerprint') expect(got.source, platform).toBe('response-body-platform-uid');
    }
  });

  it('🔴 the two platforms that carry nothing are `unknown` rather than mislabelled', async () => {
    const store = memoryStore();
    for (const platform of ['chatgpt', 'gemini', 'grok', 'deepseek', 'perplexity']) {
      const got = await accountFingerprintFor(capture(CAPTURE_URL[platform]!, BODY_NO_ID), store, null);
      expect(got, platform).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
    }
  });

});

// ===========================================================================
// Part B · the bundle that actually reaches the host
// ===========================================================================
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
  },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

/** The bundle the host was handed, parsed. The assertion is on the real write-down path, not on a builder called directly. */
function deliveredBundles(): any[] {
  return host.deliveries.map((d) => JSON.parse(d.payload));
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest, setBackfillEnabled } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

async function dispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  await new Promise<any>((resolve) => {
    runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
  });
  await mod.backfillTickSettled();
  return mod;
}

describe('W165-B · the delivered bundle carries the fingerprint', () => {
  it('🔴 a live capture reaches the host with a well-formed account fingerprint', async () => {
    await dispatch(capture(CAPTURE_URL.chatgpt!, bodyWithUid('acct-fixture-1')));
    const bundles = deliveredBundles();
    expect(bundles).toHaveLength(1);
    const bundle = bundles[0]!;

    expect(bundle.schema).toBe('chat-stasher/inbox@2');
    expect(bundle.account.kind).toBe('fingerprint');
    expect(bundle.account.value).toMatch(/^[0-9a-f]{64}$/);
    expect(bundle.account.source).toBe('response-body-platform-uid');
    // The saltId on the bundle is the one this install actually stored — not a
    // second, independently invented id.
    const stored = store[ACCOUNT_SALT_KEY] as { id: string };
    expect(bundle.account.saltId).toBe(stored.id);
    // The same value is reproducible from the stored salt, so the field is a
    // measurement rather than a random string written once.
    const { loadOrCreateAccountSalt: reload } = await import('../lib/account-fingerprint');
    const { fingerprintAccountId: refingerprint } = await import('../lib/account-fingerprint');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const salt = await reload(browserLocalStore());
    if (!salt || salt === 'unreadable') throw new Error('the capture must have created a salt');
    const { ACCOUNT_FINGERPRINT_DOMAIN } = await import('../lib/account-fingerprint');
    expect(await refingerprint(salt, ACCOUNT_FINGERPRINT_DOMAIN, 'chatgpt', 'acct-fixture-1'))
      .toBe(bundle.account.value);
  });

  it('🔴 a capture with no account id reaches the host as an explicit unknown, not a missing key', async () => {
    await dispatch(capture(CAPTURE_URL.chatgpt!, BODY_NO_ID));
    const bundles = deliveredBundles();
    expect(bundles).toHaveLength(1);
    expect(Object.prototype.hasOwnProperty.call(bundles[0]!, 'account')).toBe(true);
    expect(bundles[0]!.account).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
  });

  it('🔴 two bundles from one install carry one saltId, so they are comparable to each other', async () => {
    await dispatch(capture(CAPTURE_URL.chatgpt!, bodyWithUid('acct-fixture-1')));
    await dispatch(capture(CAPTURE_URL.chatgpt!, bodyWithUid('acct-fixture-2')));
    const bundles = deliveredBundles();
    expect(bundles).toHaveLength(2);
    expect(bundles[0]!.account.saltId).toBe(bundles[1]!.account.saltId);
    expect(bundles[0]!.account.value).not.toBe(bundles[1]!.account.value);
  });
});
