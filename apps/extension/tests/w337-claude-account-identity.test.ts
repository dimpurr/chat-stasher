import { describe, expect, it } from 'vitest';
import { readClaudeAccountId } from '../lib/claude-account';
import { accountFingerprintFor, accountIdFromCapture } from '../lib/account-fingerprint';
import { ACCOUNT_SALT_KEY } from '../lib/account-fingerprint';
import type { CapturedFetch } from '../lib/contract';
import { memoryStore } from '../lib/backfill/store';

const ORIGIN = 'https://claude.ai';
const ORG = 'aaaaaaaa-1111-2222-3333-444444444444';
const SID = 'conversation-fixture-001';
const USER_A = 'user-fixture-alpha';
const USER_B = 'user-fixture-beta';
const URL = `${ORIGIN}/api/organizations/${ORG}/chat_conversations/${SID}?tree=True`;

function reply(status: number, value?: unknown) {
  return { status, text: async () => JSON.stringify(value) };
}

describe('W337 · Claude current-user identity', () => {
  it('uses a cache-disabled who-am-I id before the settings fallback', async () => {
    const calls: Array<{ url: string; init: unknown }> = [];
    const reading = await readClaudeAccountId(URL, ORIGIN, async (url, init) => {
      calls.push({ url, init });
      return reply(200, { user: { uuid: USER_A } });
    });
    expect(reading).toEqual({ kind: 'id', id: USER_A, source: 'response-body-claude-whoami' });
    expect(calls).toHaveLength(1);
    expect(calls[0]?.url).toBe(`${ORIGIN}/api/account`);
    expect(calls[0]?.init).toMatchObject({ method: 'GET', credentials: 'same-origin', cache: 'no-store' });
  });

  it('falls back to the organization user_settings userId', async () => {
    const calls: string[] = [];
    const reading = await readClaudeAccountId(URL, ORIGIN, async (url) => {
      calls.push(url);
      return calls.length === 1 ? reply(404) : reply(200, { userId: USER_B });
    });
    expect(calls).toEqual([
      `${ORIGIN}/api/account`,
      `${ORIGIN}/api/claude_code/organizations/${ORG}/user_settings`,
    ]);
    expect(reading).toEqual({ kind: 'id', id: USER_B, source: 'response-body-claude-user-settings' });
  });

  it('keeps missing and unreadable identity as different unknown states', async () => {
    expect(await readClaudeAccountId(URL, ORIGIN, async () => reply(404)))
      .toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
    expect(await readClaudeAccountId(URL, ORIGIN, async () => ({ status: 200, text: async () => { throw new Error('synthetic unreadable body'); } })))
      .toEqual({ kind: 'unknown', reason: 'account-id-unreadable' });
  });

  it('fingerprints per install, preserves provenance, and exposes no raw id', async () => {
    const store = memoryStore();
    const capture = (id: string): CapturedFetch => ({
      url: URL, method: 'GET', status: 200, text: '{}', capturedAt: 1,
      claudeAccountId: id,
      claudeAccountIdSource: 'response-body-claude-whoami',
    });
    const one = await accountFingerprintFor(capture(USER_A), store, SID);
    const repeat = await accountFingerprintFor(capture(USER_A), store, SID);
    const other = await accountFingerprintFor(capture(USER_B), store, SID);
    expect(accountIdFromCapture(capture(USER_A), SID)).toEqual({
      kind: 'id', id: USER_A, source: 'response-body-claude-whoami',
    });
    expect(one.kind).toBe('fingerprint');
    if (one.kind !== 'fingerprint' || repeat.kind !== 'fingerprint' || other.kind !== 'fingerprint') throw new Error('synthetic fixture must fingerprint');
    expect(repeat.value).toBe(one.value);
    expect(other.value).not.toBe(one.value);
    expect(one.source).toBe('response-body-claude-whoami');
    expect(JSON.stringify(one)).not.toContain(USER_A);
    expect(JSON.stringify(store.data[ACCOUNT_SALT_KEY])).not.toContain(USER_A);
  });
});
