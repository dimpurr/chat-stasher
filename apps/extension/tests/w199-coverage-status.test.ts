/**
 * W199 · the account half of a coverage row.
 *
 * W128 step 2 asks for "a clear status on the coverage page", and clear is doing work in that
 * sentence: the page has three facts to tell apart, and two of them look identical if the row
 * simply omits what it does not have.
 *
 *  1. 🔴 **the account is known** — the scope names one and this install has a lease for it;
 *  2. 🔴 **no account has ever been recorded** — a switch here could not be detected, and the
 *     row says so instead of looking like every other row;
 *  3. 🔴 **the scope is suspended** — a state of its own, whose sentence may not borrow either
 *     neighbour's: `waiting-retry` would promise a clock the suspension does not have, and
 *     `halted` would say the leg has stopped for good when a person using that account is the
 *     whole remedy.
 *
 * The model is a pure function, so nothing here needs a browser: the header is handed in and
 * the words come out. Every value is a synthetic fixture.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import {
  accountNote,
  buildCoverage,
  stateNote,
  type CoverageInput,
  type CoverageScopeInput,
} from '../lib/coverage';
import { chipOf } from '../lib/coverage-view';
import type { BackfillHeader } from '../lib/backfill/types';

const PLATFORM = 'grok';
const SCOPE = 'acct-fixture-1';
const NOW = Date.UTC(2026, 8, 26, 12, 0, 0);

const LEASE = {
  value: 'f'.repeat(64),
  saltId: 'fixture-salt',
  source: 'response-body-platform-uid' as const,
  at: NOW - 60_000,
};

function header(over: Partial<BackfillHeader> = {}): BackfillHeader {
  return {
    v: 2,
    platform: PLATFORM,
    scope: SCOPE,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 3, complete: true },
    pendingCount: 2,
    archivedCount: 1,
    detailToday: { day: '2026-09-26', count: 0 },
    halted: null,
    ...over,
  };
}

function input(over: Partial<CoverageScopeInput> = {}): CoverageInput {
  return {
    scopes: [{
      platform: PLATFORM,
      scope: SCOPE,
      header: header(),
      debt: { pending: ['p1', 'p2'], archived: ['a1'], times: new Map() },
      registered: true,
      skippedReason: null,
      ...over,
    }],
    enabled: true,
    hostPaused: false,
    presetRaw: 'gentle',
    tick: null,
    now: NOW,
  };
}

beforeEach(() => {
  vi.stubGlobal('chrome', withI18n({}));
});

describe('W199-M · the account line', () => {
  it('🔴 a known account names the mechanism that read it, never the digest', () => {
    const row = buildCoverage(input({ header: header({ accountLease: LEASE }) })).rows[0]!;
    const note = accountNote(row);
    expect(note).toContain('response body');
    // The digest is not printed: it means nothing to a person, and printing it would invite a
    // comparison across rows that is only meaningful within one install.
    expect(note).not.toContain(LEASE.value);
    expect(note).not.toContain(LEASE.saltId);
  });

  it('🔴 a scope with no recorded account says so, rather than looking like the others', () => {
    const row = buildCoverage(input()).rows[0]!;
    expect(row.accountLease).toBeNull();
    expect(accountNote(row)).toContain('No account has ever been recorded');
  });

  it('🔴 the org mechanism gets its own wording', () => {
    const row = buildCoverage(input({
      header: header({ accountLease: { ...LEASE, source: 'request-url-organization' } }),
    })).rows[0]!;
    expect(accountNote(row)).toContain("page's own request");
  });

  it('🔴 a lease this build cannot read is reported as none, not half-read', () => {
    const row = buildCoverage(input({
      header: header({ accountLease: { value: 'v', saltId: 's', source: 'nope' as never, at: 1 } }),
    })).rows[0]!;
    expect(row.accountLease).toBeNull();
    expect(accountNote(row)).toContain('No account has ever been recorded');
  });
});

describe('W199-N · the suspended row', () => {
  const suspendedHeader = header({
    accountLease: LEASE,
    suspended: {
      at: NOW - 1000,
      reason: 'account-changed',
      lease: LEASE,
      observed: { value: 'e'.repeat(64), saltId: 'fixture-salt', source: 'response-body-platform-uid' },
    },
    // A suspended scope normally carries the halt too. The state must be the suspension's,
    // because the halt's own sentence describes a backoff that is no longer the whole story.
    halted: { reason: 'account-changed', at: NOW - 1000, detail: 'fixture', attempts: 1, retryAt: NOW + 60_000 },
  });

  it('🔴 the state is its own, not `halted`', () => {
    const row = buildCoverage(input({ header: suspendedHeader })).rows[0]!;
    expect(row.state).toBe('account-suspended');
    expect(row.suspended?.reason).toBe('account-changed');
  });

  it('🔴 the sentence promises no clock and no finality — it names the action', () => {
    const row = buildCoverage(input({ header: suspendedHeader })).rows[0]!;
    const note = stateNote(row, NOW)!;
    expect(note).toContain('different account');
    expect(note).toContain('resumes by itself once this account is used again');
    // Neither neighbour's claim: no "waiting out a backoff", no "has stopped".
    expect(note).not.toContain('backoff');
    expect(note.toLowerCase()).not.toContain('has stopped');
  });

  it('🔴 the chip is neither "stopped" nor "retrying"', () => {
    const row = buildCoverage(input({ header: suspendedHeader })).rows[0]!;
    // 🔴 Spelled out rather than read from a constant: the point is that this row gets a word
    //    of its own, and asserting `chip.tone === 'wait'` alone would pass for `retrying` too.
    expect(chipOf(row).word).toBe('holding this account\'s place');
  });

  it('🔴 a suspension this build cannot read does not silently become a running row', () => {
    const row = buildCoverage(input({
      header: header({ accountLease: LEASE, suspended: { at: 'nope' as never, reason: 'account-changed' } }),
    })).rows[0]!;
    // Not `account-suspended` (there is nothing readable to word), and not a claim about the
    // account either: the row falls through to what the rest of the header says, and the halt
    // that is normally beside a suspension is what a reader acts on. The point of this case is
    // that no path here *invents* an account fact out of an unreadable record.
    expect(row.suspended).toBeNull();
    expect(row.state).not.toBe('account-suspended');
  });
});
