import { describe, expect, it } from 'vitest';
import { claudeWhoAmIId, readClaudeAccountId, withClaudeAccountReading } from '../lib/claude-account';
import { accountFingerprintFor, accountIdFromCapture } from '../lib/account-fingerprint';
import { ACCOUNT_SALT_KEY } from '../lib/account-fingerprint';
import type { CapturedFetch } from '../lib/contract';
import { memoryStore } from '../lib/backfill/store';
import { runBackfill, type HttpPort, type HttpResponse } from '../lib/backfill/engine';
import { CLAUDE_ACCOUNT_IDENTITY_MESSAGE, tabHttpPort } from '../lib/backfill/tab-port';
import type { Clock } from '../lib/backfill/pace';

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

  it('does not treat ambiguous top-level id or uuid fields as the current user', () => {
    expect(claudeWhoAmIId({ id: 'project-fixture' })).toBeNull();
    expect(claudeWhoAmIId({ uuid: 'organization-fixture' })).toBeNull();
    expect(claudeWhoAmIId({ id: 'project-fixture', uuid: 'organization-fixture' })).toBeNull();
    expect(claudeWhoAmIId({ user: { uuid: USER_A } })).toBe(USER_A);
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

  it('discards page-supplied Claude identity when the trusted lookup is unknown', async () => {
    const pageCapture: CapturedFetch = {
      url: URL, method: 'GET', status: 200, text: '{}', capturedAt: 1,
      claudeAccountId: USER_B,
      claudeAccountIdSource: 'response-body-claude-whoami',
    };
    const trusted = withClaudeAccountReading(pageCapture, { kind: 'unknown', reason: 'no-account-id-in-capture' });
    expect(trusted).not.toHaveProperty('claudeAccountId');
    expect(trusted).not.toHaveProperty('claudeAccountIdSource');
    expect(trusted.claudeAccountUnknownReason).toBe('no-account-id-in-capture');
    expect(accountIdFromCapture(trusted, SID)).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
  });

  it('asks the owning tab for a Claude identity through the dedicated message', async () => {
    const messages: unknown[] = [];
    const port = tabHttpPort(7, async (tabId, message) => {
      expect(tabId).toBe(7);
      messages.push(message);
      return { kind: 'id', id: USER_A, source: 'response-body-claude-whoami' };
    });
    expect(await port.claudeAccountIdentity?.(URL)).toEqual({
      kind: 'id', id: USER_A, source: 'response-body-claude-whoami',
    });
    expect(messages).toEqual([{ type: CLAUDE_ACCOUNT_IDENTITY_MESSAGE, url: URL }]);
  });

  it('asks the page identity port for each Claude backfill capture before the archive sink', async () => {
    const store = memoryStore();
    const lookedUpUrls: string[] = [];
    const captures: CapturedFetch[] = [];
    const detailUrl = `${ORIGIN}/api/organizations/${ORG}/chat_conversations/${SID}?tree=True&rendering_mode=messages&render_all_tools=true`;
    const body = JSON.stringify({
      uuid: SID,
      name: 'synthetic conversation',
      model: 'synthetic-model',
      current_leaf_message_uuid: 'message-fixture-1',
      chat_messages: [{ uuid: 'message-fixture-1', index: 0, sender: 'human', content: [{ type: 'text', text: 'synthetic body' }] }],
    });
    const http: HttpPort = async (url): Promise<HttpResponse> => {
      if (url.startsWith(`${ORIGIN}/api/organizations/${ORG}/chat_conversations?`)) {
        return { status: 200, text: JSON.stringify([{ uuid: SID, name: 'synthetic conversation' }]) };
      }
      if (url.startsWith(`${ORIGIN}/api/organizations/${ORG}/chat_conversations/${SID}?`)) return { status: 200, text: body };
      throw new Error(`unexpected synthetic URL: ${new globalThis.URL(url).pathname}`);
    };
    http.claudeAccountIdentity = async (captureUrl) => {
      lookedUpUrls.push(captureUrl);
      // A later account is not reused for an earlier delivered URL: each capture
      // gets its own reading from the owning page rather than a cached id.
      return { kind: 'id', id: USER_A, source: 'response-body-claude-whoami' };
    };
    let now = 1_790_000_000_000;
    const clock: Clock = { now: () => now++, async sleep() { /* no waiting */ } };
    const report = await runBackfill({
      platform: 'claude', origin: ORIGIN, scope: ORG, store, http, clock,
      pace: { enumerate: { minIntervalMs: 0, maxPerDay: null }, detail: { minIntervalMs: 0, maxPerDay: null } },
      random: () => 0, maxDetails: 1,
      sink: (captured) => { captures.push(captured); return { saved: true, sessionId: SID }; },
    });
    expect(report.halted).toBeNull();
    expect(lookedUpUrls).toEqual([detailUrl]);
    expect(captures).toHaveLength(1);
    expect(captures[0]).toMatchObject({
      claudeAccountId: USER_A,
      claudeAccountIdSource: 'response-body-claude-whoami',
    });
  });
});
