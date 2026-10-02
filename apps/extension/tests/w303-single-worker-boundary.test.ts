import { readFileSync } from 'node:fs';
import { describe, expect, expectTypeOf, it } from 'vitest';
import type { FingerprintedChatGptIdentity, FingerprintedChatGptWorkspace } from '../lib/backfill/chatgpt-workspace';
import { fingerprintChatGptIdentityAtWorkerBoundary } from '../lib/backfill/tab-port';
import type { AccountIdentity } from '../lib/backfill/types';

const contentSource = readFileSync(new URL('../entrypoints/dw-bridge.content.ts', import.meta.url), 'utf8');
const authSource = readFileSync(new URL('../lib/platform-auth.ts', import.meta.url), 'utf8');
const tabPortSource = readFileSync(new URL('../lib/backfill/tab-port.ts', import.meta.url), 'utf8');
const backgroundSource = readFileSync(new URL('../entrypoints/background.ts', import.meta.url), 'utf8');

describe('W303c · one first-worker boundary for raw ChatGPT account ids', () => {
  it('keeps one current raw header slot and fingerprints each sent value at the worker boundary', () => {
    expect(contentSource).not.toContain('accountIds');
    expect(contentSource).not.toContain('observeChatGptAccountId');
    expect(contentSource).toContain('chatGptCurrentRawHeader');
    expect(contentSource).not.toContain('chatGptCurrentRequestIdentity');
    expect(contentSource.match(/let chatGptCurrentRawHeader:/g)).toHaveLength(1);
    expect(contentSource).toContain("window.addEventListener('pagehide', clearChatGptAccountHeader)");
    expect(contentSource).toContain("window.addEventListener('unload', clearChatGptAccountHeader)");
    expect(contentSource).toContain('generation !== chatGptObservationGeneration');
    expect(contentSource).not.toContain('currentAccountId');
    const observationReply = contentSource.match(/\.then\(\(reply: unknown\) => \{([\s\S]*?)\n\s*\}\)\s*\.catch/)?.[1];
    expect(observationReply).toBeTruthy();
    expect(observationReply).not.toContain('currentAccountId');
    expect(contentSource).toMatch(/runtime\.sendMessage\(\{\s*type:\s*CHATGPT_WORKSPACE_OBSERVED_MESSAGE,\s*accountId\s*\}\)/);
    expect(contentSource).toContain('observeChatGptWorkspaceFingerprint(chatGptWorkspaceObservation, safeIdentity)');
    expect(contentSource).toContain('value: chatGptCurrentRawHeader');
    expect(contentSource).toContain('generation: chatGptObservationGeneration');
    expect(contentSource).toContain('chatgptAccountIdHeader: answer.response.chatgptAccountIdHeader');
    expect(authSource).toContain("throw new Error('chatgpt-account-header-unavailable')");
    expect(authSource).toContain("throw new Error('chatgpt-account-header-changed-during-retry')");
    expect(authSource).toContain("if ((error as Error).message === 'chatgpt-account-header-changed-during-retry') return first");
    expect(authSource).toContain("headers['ChatGPT-Account-Id'] = current.value");
    expect(backgroundSource).toContain('fingerprintChatGptIdentityAtWorkerBoundary(');
    expect(backgroundSource).not.toContain('fingerprintChatGptWorkspace');
    expect(backgroundSource).not.toMatch(/workspace\.workspace[^;]*fingerprint/);
    const projection = tabPortSource.slice(tabPortSource.indexOf('async function httpResponseFromBackfillReplyAtWorkerBoundary'));
    expect(projection.indexOf('delete reply.chatgptAccountIdHeader')).toBeGreaterThan(-1);
    expect(projection.indexOf('delete reply.chatgptAccountIdHeader'))
      .toBeLessThan(projection.indexOf('await fingerprintChatGptIdentityAtWorkerBoundary(rawHeader'));
    expect(projection).not.toMatch(/response\.chatgptAccountIdHeader\s*=/);
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
