/**
 * W239 · **W128 step 3 — an organization is not an account.**
 *
 * ## The gap this pins, in the words it was found in
 *
 * claude.ai addresses every conversation by **organization** (`/api/organizations/<org>/
 * chat_conversations/…`), so the organization is what this build's Claude scope is built
 * from, and it is also what the page-side guard compares on every request
 * (`lib/backfill/claude-page.ts`'s `allowed`). That guard closes the *cross-organization*
 * case: an account switch that changes the active organization halts before a request.
 *
 * It cannot close the **same-organization** case. Two accounts can be members of one
 * organization (a Team/Enterprise workspace; `W199-OUT.md` §4 records the same failure
 * in production in an unrelated project — farion1231/cc-switch v3.20.1, where managed
 * accounts keyed by `chatgpt_account_id` "identifies a ChatGPT workspace rather than a
 * person" collapsed into one record). For those two accounts the organization is the same
 * value, so nothing derived from the organization can tell them apart.
 *
 * ## Why the field had to change rather than be compared
 *
 * Step 1's contract for the bundle's `account` field is explicit about what makes it
 * meaningful: *"the same account on the same install hashes the same string every time,
 * and a different account hashes a different one. If either half fails the field is worse
 * than absent, because it would be read as evidence about an account."*
 *
 * For an organization-scoped plan the second half fails **by construction** — the value is
 * a function of the organization alone, and two accounts share one. A bundle captured
 * under B then carries A's value, which is not "unknown": it is a positive, false
 * statement that these two conversations came from the same account. So this change stops
 * producing that value, names the fact (`organization-is-not-an-account`), and leaves the
 * organization where it is a genuine fact rather than an account claim — in the bundle's
 * own `url`, and as the request namespace the host coordinates under (D1).
 *
 * ## What this file pins
 *
 *  1. 🔴 **the bundle records `unknown`** for an organization-scoped capture — and creates
 *     no salt, because nothing is keyed with one there;
 *  2. 🔴 **the lease refuses to exist over an organization**, with its own reason rather
 *     than the generic "this platform's plan declares no account", and a lease already
 *     recorded on an old header cannot put a run back under one;
 *  3. 🔴 **the coverage row says the third fact out loud** — "an organization is not a
 *     person" is not "no account was ever looked for";
 *  4. 🔴 **the organization is still the request namespace** the host's per-namespace
 *     budget is keyed by (D1), so this is a refusal to call it an account, not a removal;
 *  5. 🔴 **the two declarations cannot drift**: everything declared organization-scoped
 *     really does address its conversations by a path segment, and no platform is both
 *     leased and organization-scoped.
 *
 * Zero network, zero logged-in state, zero real account data: every id below is an obvious
 * fixture string.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import type { CapturedFetch } from '../lib/contract';
import {
  ACCOUNT_SALT_KEY,
  accountFingerprintFor,
  accountIdFromCapture,
  coordinationIdFromCapture,
} from '../lib/account-fingerprint';
import {
  ORGANIZATION_SCOPED_PLATFORMS,
  backfillPlanFor,
  planScopeIsOrganization,
} from '../lib/backfill/enumerate';
import { accountLeaseForScope, decideRunLease, planHoldsAccountLease } from '../lib/backfill/account-lease';
import { accountNote, buildCoverage, type CoverageInput } from '../lib/coverage';
import { memoryStore } from '../lib/backfill/store';
import type { BackfillHeader } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// Synthetic fixtures. No real account id, org id or conversation id appears
// below; every one is an obvious fixture string.
// ---------------------------------------------------------------------------
const ORG_A = '11111111-2222-3333-4444-555555555555';
const SID = 'aaaaaaaa-1111-2222-3333-444444444444';

const CLAUDE_URL = (org: string): string =>
  `https://claude.ai/api/organizations/${org}/chat_conversations/${SID}?tree=True`;
/** A conversation body carrying nothing account-shaped at all. */
const BODY_NO_ID = JSON.stringify({ mapping: {}, current_node: 'n0' });
/** A body that carries an account-shaped id — the ADR-002 account axis. */
const bodyWithUid = (uid: string): string => JSON.stringify({ account_id: uid, mapping: {}, current_node: 'n0' });
const CHATGPT_URL = `https://chatgpt.com/backend-api/conversation/${SID}`;

function capture(url: string, text: string): CapturedFetch {
  return { url, method: 'GET', status: 200, text, capturedAt: 1_700_000_000_000 };
}

/** The reason every reader of a Claude capture must give. One spelling, asserted once. */
const ORG_REASON = 'organization-is-not-an-account';

beforeEach(() => {
  vi.stubGlobal('chrome', withI18n({}));
});

describe('W239-A · the bundle records no account fingerprint over an organization', () => {
  it('🔴 a Claude capture is `unknown` with the reason, never a value a reader would take for an account', async () => {
    const store = memoryStore();
    const got = await accountFingerprintFor(capture(CLAUDE_URL(ORG_A), BODY_NO_ID), store, null);
    expect(got).toEqual({ kind: 'unknown', reason: ORG_REASON });
  });

  it('🔴 and it keys nothing with a secret: no salt is created, and nothing is written', async () => {
    // A salt exists to make a fingerprint unrecoverable. Where no fingerprint is produced
    // there is nothing to key, and creating the install's secret anyway would be a secret
    // kept for no reader — the store's own write counter is what makes that observable.
    const store = memoryStore();
    await accountFingerprintFor(capture(CLAUDE_URL(ORG_A), BODY_NO_ID), store, null);
    expect(await store.load(ACCOUNT_SALT_KEY)).toBeNull();
    expect(store.writes).toBe(0);
  });

  it('🔴 the id reader makes the same statement as the field it feeds', () => {
    // Two readers of one decision: a capture cannot be "an account" for the bundle and
    // "not an account" for the engine's attribution check.
    expect(accountIdFromCapture(capture(CLAUDE_URL(ORG_A), BODY_NO_ID), null))
      .toEqual({ kind: 'unknown', reason: ORG_REASON });
  });

  it('🔴 the refusal is the plan’s, not the string "claude": every organization-scoped plan is covered', async () => {
    // Written against the declaration rather than against one platform id, so a second
    // path-scoped plan cannot be added without landing on this rule.
    for (const platform of ORGANIZATION_SCOPED_PLATFORMS) {
      expect(planScopeIsOrganization(platform)).toBe(true);
    }
    expect(planScopeIsOrganization('claude')).toBe(true);
    expect(ORGANIZATION_SCOPED_PLATFORMS).toContain('claude');
  });
});

describe('W239-B · the organization is still the request namespace, which is not an account', () => {
  it('🔴 the coordination id is the organization for Claude and the body axis elsewhere', () => {
    // D1's budget is per *namespace*, and an organization genuinely is the namespace every
    // Claude request addresses. That is why the value is still computed and still sent —
    // and why it is no longer the value recorded as an account.
    expect(coordinationIdFromCapture(capture(CLAUDE_URL(ORG_A), BODY_NO_ID), null)).toBe(ORG_A);
    expect(coordinationIdFromCapture(capture(CHATGPT_URL, bodyWithUid('acct-fixture-1')), null)).toBe('acct-fixture-1');
    // No id visible ⇒ nothing invented: the host hears "no account id", which is a state
    // its own contract already has.
    expect(coordinationIdFromCapture(capture(CHATGPT_URL, BODY_NO_ID), null)).toBeNull();
  });

  it('🔴 the two declarations are disjoint, and every declared plan really is path-scoped', () => {
    for (const platform of ORGANIZATION_SCOPED_PLATFORMS) {
      expect(backfillPlanFor(platform)?.scopeInPath, `${platform} must address conversations by a path segment`).toBeTruthy();
    }
    expect(ORGANIZATION_SCOPED_PLATFORMS.filter((platform) => planHoldsAccountLease(platform))).toEqual([]);
  });
});

describe('W239-C · no run can lease an organization', () => {
  it('🔴 the scope is unleased for its own stated reason, not the generic one', async () => {
    const store = memoryStore();
    expect(await accountLeaseForScope('claude', ORG_A, store, 1))
      .toEqual({ kind: 'unleased', reason: 'scope-names-an-organization' });
    // And the reason is not the one for a platform with no account axis at all: "the plan
    // does not declare a scope" is a false statement about this plan.
    expect(await accountLeaseForScope('chatgpt', 'acct-fixture-1', store, 1))
      .toEqual({ kind: 'unleased', reason: 'platform-not-scoped' });
  });

  it('🔴 a lease already recorded on an old header cannot put a run back under an organization', async () => {
    const recorded = {
      value: 'f'.repeat(64),
      saltId: 'fixture-salt',
      source: 'request-url-organization' as const,
      at: 1,
    };
    const reading = await accountLeaseForScope('claude', ORG_A, memoryStore(), 2);
    expect(decideRunLease(recorded, reading))
      .toEqual({ kind: 'unleased', reason: 'scope-names-an-organization' });
  });
});

describe('W239-D · the coverage row tells the three facts apart', () => {
  function claudeRow(over: Partial<BackfillHeader> = {}) {
    const input: CoverageInput = {
      scopes: [{
        platform: 'claude',
        scope: ORG_A,
        header: {
          v: 2,
          platform: 'claude',
          scope: ORG_A,
          totalKnown: null,
          totalSource: 'unknown',
          enumCursor: { offset: 3, complete: true },
          pendingCount: 2,
          archivedCount: 1,
          detailToday: { day: '2026-09-26', count: 0 },
          halted: null,
          ...over,
        },
        debt: { pending: ['p1', 'p2'], archived: ['a1'], times: new Map() },
        registered: true,
        skippedReason: null,
      }],
      enabled: true,
      hostPaused: false,
      presetRaw: 'gentle',
      tick: null,
      now: Date.UTC(2026, 8, 28, 12, 0, 0),
    };
    return buildCoverage(input).rows[0]!;
  }

  it('🔴 a Claude row says an organization is not a person, rather than "nothing was ever recorded"', () => {
    const note = accountNote(claudeRow());
    expect(note).toContain('organization');
    expect(note).toContain('cannot be told apart');
    // The other sentence's claim — that no account was ever recorded here *because the
    // platform's responses are limited* — is a different fact, and borrowing it would
    // describe this scope as though the extension had simply seen nothing.
    expect(note).not.toContain('No account has ever been recorded');
  });

  it('🔴 a lease recorded on an old Claude header is not printed as an account either', () => {
    // Old headers carry a lease taken over the organization. Reading it back as "the
    // account this scope belongs to" is exactly the claim this change removes, so the row
    // states the organization limit whatever the record says.
    const row = claudeRow({
      accountLease: { value: 'f'.repeat(64), saltId: 'fixture-salt', source: 'request-url-organization', at: 1 },
    });
    expect(accountNote(row)).not.toContain('Account this scope belongs to');
    expect(accountNote(row)).toContain('cannot be told apart');
  });

  it('🔴 the other platforms keep their own sentence', () => {
    // The change is about the plan's scope axis, not about the page: a leased platform
    // still names the mechanism that read its account.
    const note = accountNote({
      ...claudeRow(),
      platform: 'grok',
      accountLease: { value: 'f'.repeat(64), saltId: 'fixture-salt', source: 'response-body-platform-uid', at: 1 },
    });
    expect(note).toContain('Account this scope belongs to');
    expect(note).toContain('response body');
  });
});

describe('W239-E · the organization claim does not leave this machine', () => {
  const NOW = Date.UTC(2026, 8, 28, 12, 0, 0);
  const ORG_LEASE = {
    value: 'f'.repeat(64),
    saltId: 'fixture-salt',
    source: 'request-url-organization' as const,
    at: 1,
  };

  /** One row, with whatever lease an older build recorded on that scope's header. */
  function rowWithLease(platform: string, scope: string, accountLease?: BackfillHeader['accountLease']) {
    const input: CoverageInput = {
      scopes: [{
        platform,
        scope,
        header: {
          v: 2,
          platform,
          scope,
          totalKnown: null,
          totalSource: 'unknown',
          enumCursor: { offset: 3, complete: true },
          pendingCount: 2,
          archivedCount: 1,
          detailToday: { day: '2026-09-28', count: 0 },
          halted: null,
          ...(accountLease ? { accountLease } : {}),
        },
        debt: { pending: ['p1', 'p2'], archived: ['a1'], times: new Map() },
        registered: true,
        skippedReason: null,
      }],
      enabled: true,
      hostPaused: false,
      presetRaw: 'gentle',
      tick: null,
      now: NOW,
    };
    return buildCoverage(input).rows[0]!;
  }

  it('🔴 a lease taken over an organization is not handed on as this scope\'s account', () => {
    // The row's `accountLease` is what the install status report forwards to the host as
    // `account_fingerprint` (`entrypoints/background.ts`, the per-platform grouping): it
    // reads `row.accountLease?.value` and sends it, so a value still sitting here is a
    // value that leaves the machine. An old header is the one place this label survives —
    // the page sentence is already answered from the plan — and what it holds is an
    // organization, which is not an account on this machine or anywhere else.
    const row = rowWithLease('claude', ORG_A, ORG_LEASE);
    expect(row.accountLease).toBeNull();
  });

  it('🔴 a platform whose scope is a person still reports its lease', () => {
    // The rule is about the scope's axis, not about old records: a leased platform must
    // keep reporting the account it recorded, or the report would lose a real answer.
    const row = rowWithLease('grok', 'acct-fixture-1', { ...ORG_LEASE, source: 'response-body-platform-uid' });
    expect(row.accountLease?.value).toBe('f'.repeat(64));
    expect(row.accountLease?.source).toBe('response-body-platform-uid');
  });
});
