import { readFileSync } from 'node:fs';
import { describe, expect, expectTypeOf, it } from 'vitest';
import type { FingerprintedChatGptIdentity, FingerprintedChatGptWorkspace } from '../lib/backfill/chatgpt-workspace';
import { fingerprintChatGptIdentityAtWorkerBoundary } from '../lib/backfill/tab-port';
import type { AccountIdentity } from '../lib/backfill/types';

const contentSource = readFileSync(new URL('../entrypoints/dw-bridge.content.ts', import.meta.url), 'utf8');
const tabPortSource = readFileSync(new URL('../lib/backfill/tab-port.ts', import.meta.url), 'utf8');
const backgroundSource = readFileSync(new URL('../entrypoints/background.ts', import.meta.url), 'utf8');

describe('W303c · one first-worker boundary for raw ChatGPT account ids', () => {
  it('keeps only fingerprints on the long-lived content observation', () => {
    expect(contentSource).not.toContain('accountIds');
    expect(contentSource).not.toContain('observeChatGptAccountId');
    expect(contentSource).toContain('chatGptCurrentRawHeader');
    expect(contentSource).toMatch(/runtime\.sendMessage\(\{\s*type:\s*CHATGPT_WORKSPACE_OBSERVED_MESSAGE,\s*accountId\s*\}\)/);
    expect(contentSource).toContain('observeChatGptWorkspaceFingerprint(chatGptWorkspaceObservation, safeIdentity)');
    expect(contentSource).toContain("'ChatGPT-Account-Id': requestIdentity.accountId");
    expect(contentSource).toContain('chatgptAccountIdentity: requestIdentity.identity');
    expect(contentSource).toContain("throw new Error('chatgpt-account-header-unavailable')");
    expect(backgroundSource).toContain('fingerprintChatGptIdentityAtWorkerBoundary(');
    expect(backgroundSource).not.toContain('fingerprintChatGptWorkspace');
    expect(backgroundSource).not.toMatch(/workspace\.workspace[^;]*fingerprint/);
  });

  it('exports a branded fingerprint from the boundary and only branded workspace results from tabHttpPort', () => {
    expect(tabPortSource).toContain('Promise<FingerprintedChatGptIdentity | null>');
    expect(tabPortSource).toContain('Promise<ChatGptWorkspaceResolution>');
    expect(tabPortSource).not.toMatch(/chatgptAccountIdHeader\?\s*:\s*string/);
    expect(tabPortSource).not.toMatch(/export\s+(?:type|interface)\s+RawChatGpt/);
    expect(tabPortSource).not.toMatch(/port\.chatgptWorkspace[\s\S]{0,900}workspace:\s*reply\.workspace\s*,\s*observed/);
    expectTypeOf<string>().not.toMatchTypeOf<FingerprintedChatGptWorkspace>();
    expectTypeOf<AccountIdentity>().not.toMatchTypeOf<FingerprintedChatGptIdentity>();
    expectTypeOf<ReturnType<typeof fingerprintChatGptIdentityAtWorkerBoundary>>()
      .toMatchTypeOf<Promise<FingerprintedChatGptIdentity | null>>();
  });
});
