/**
 * W931 · **What a ChatGPT backfill run must record when the page cannot supply
 * the request-local account identity.**
 *
 * Measured in the dedicated test browser on 2026-10-07 (extension `0.2.0.25`,
 * `origin/main` b4ec4a62): the body request is refused **before it is sent** —
 * `lib/platform-auth.ts`'s `withToken` throws `chatgpt-account-header-unavailable`
 * when the content script's observed-header slot is empty, and it is empty on any
 * profile whose ChatGPT session is gone, because the page then never makes a
 * `/backend-api/` request to carry the header. `detailToday.count` stays at 0 and
 * `lastFetchAt.detail` moves on every attempt: that pair is the signature of "the
 * body segment was entered and its first request was refused".
 *
 * What this file pins is the *record* that refusal must leave, because the two
 * halves of it are what an operator acts on:
 *   · the reason must be the account-identity stop, not a bare transport error —
 *     "we did not send anything because we could not name the account" and "the
 *     platform dropped a request" are different facts and only one of them is
 *     true here;
 *   · the scope must be **suspended**, so the leg holds instead of spending its
 *     transient ladder on a condition only a human (a sign-in) can clear. See
 *     docs-dev/threat-model.md, "If the slot is empty, the request is not sent and
 *     the scope is suspended."
 *
 * Zero network, zero logged-in state, zero real data: every id is an obvious
 * fixture and the port is a pure function that throws on any path it was not
 * given. The identity fixture is the same shape the worker boundary hands the
 * engine (`request-header-chatgpt-account-id`), never a real account value.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpPort, type HttpResponse } from '../lib/backfill/engine';
import { openLedger } from '../lib/backfill/ledger';
import { memoryStore } from '../lib/backfill/store';
import type { Clock } from '../lib/backfill/pace';
import { chatGptWorkspaceScope, fingerprintedChatGptWorkspace } from '../lib/backfill/chatgpt-workspace';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};
const C1 = 'cc111111-0000-4000-8000-000000000001';
const FIXTURE_WORKSPACE = 'fixture-workspace';
const FIXTURE_IDENTITY = {
  value: 'a'.repeat(64),
  saltId: 'fixture-salt-id',
  source: 'request-header-chatgpt-account-id' as const,
};

function fakeClock(): Clock {
  let t = Date.parse('2026-10-07T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

function header(store: ReturnType<typeof memoryStore>, scope: string): BackfillHeader {
  return store.data[stateKey('chatgpt', scope)] as unknown as BackfillHeader;
}

/** A ledger with the enumeration finished and one body still owed. */
async function seededLedger(store: ReturnType<typeof memoryStore>, scope: string) {
  const opened = await openLedger(store, 'chatgpt', scope);
  if (!opened.ok) throw new Error('fixture must open the ChatGPT ledger');
  opened.state.enumCursor.complete = true;
  opened.state.pending = [C1];
  await opened.ledger.save(opened.state);
}

describe('W931 · ChatGPT runs that cannot observe the account header', () => {
  const scope = chatGptWorkspaceScope(FIXTURE_WORKSPACE)!;

  it('a body request the page refuses leaves the account-identity stop and suspends the scope', async () => {
    const store = memoryStore();
    await seededLedger(store, scope);

    const calls: string[] = [];
    const http: HttpPort = Object.assign(async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      // Exactly what the page-side wrapper throws when its observed-header slot is empty.
      throw new Error('chatgpt-account-header-unavailable');
    }, {
      chatgptWorkspace: async () => ({
        ok: true as const, workspace: fingerprintedChatGptWorkspace(FIXTURE_IDENTITY.value),
        identity: FIXTURE_IDENTITY, observed: true as const,
      }),
      chatgptAccountIdentity: async () => FIXTURE_IDENTITY,
    });

    const report = await runBackfill({
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope, store,
      http, clock: fakeClock(), pace: NO_WAIT,
    });

    // The request was attempted and refused before it left the page: one call, no body read.
    expect(calls).toHaveLength(1);
    expect(report.state.detailToday?.count).toBe(0);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.state.pending).toEqual([C1]);
    // The stop names the identity, not the transport.
    expect(report.halted?.reason).toBe('refused-unknown');
    expect(report.halted?.detail).toContain('no current ChatGPT account header was available');
    // …and the scope holds until its own account is used again.
    expect(header(store, scope).suspended).toMatchObject({ reason: 'request-refused' });
  });

  it('a page that has never observed a header stops before any request, and suspends', async () => {
    const store = memoryStore();
    await seededLedger(store, scope);

    const calls: string[] = [];
    const http: HttpPort = Object.assign(async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      throw new Error(`fixture must not fetch: ${url}`);
    }, {
      chatgptWorkspace: async () => ({
        ok: false as const, reason: 'workspace-unresolved' as const, observed: false,
      }),
      chatgptAccountIdentity: async () => null,
    });

    const report = await runBackfill({
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope, store,
      http, clock: fakeClock(), pace: NO_WAIT,
    });

    // Nothing was issued at all — the run stopped at the preflight.
    expect(calls).toEqual([]);
    expect(report.halted?.reason).toBe('refused-unknown');
    expect(header(store, scope).suspended).toMatchObject({ reason: 'request-refused' });
    expect(report.state.pending).toEqual([C1]);
  });
});
