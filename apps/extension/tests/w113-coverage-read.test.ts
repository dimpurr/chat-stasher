/**
 * W113 · ADR-032 — the reading half: which records the page accepts, and how a key becomes a pair.
 *
 * Two of these cases are about a shape that is coming rather than one that is here. ADR-031 makes the
 * ChatGPT ledger key `chatgpt:<workspace-id>`, so a scope may **contain a colon**; splitting on the last
 * colon, or on all of them, would silently truncate the workspace and then use the truncated string as a
 * map key — a wrong answer with no error anywhere. The scope is whatever follows the *first* colon, and
 * that is asserted with a colon-bearing scope.
 *
 * The rest is the same discipline the popup applies to a stored header: a record whose own identity
 * disagrees with the key it was found under, or a value that is not a header at all, is not progress to
 * show a user.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import {
  HEADER_KEY_PREFIX,
  isReadableHeaderAt,
  keyOfRow,
  splitHeaderKey,
  targetsOf,
} from '../lib/coverage-read';
import { withI18n } from './i18n-harness';
import { initialState, stateKey, type BackfillHeader } from '../lib/backfill/types';
import { BACKFILL_TARGETS_KEY } from '../lib/backfill/alarm';

function header(over: Partial<BackfillHeader> = {}): BackfillHeader {
  const base = initialState('chatgpt', 'default');
  return {
    v: 2,
    platform: 'chatgpt',
    scope: 'default',
    totalKnown: base.totalKnown,
    totalSource: base.totalSource,
    enumCursor: base.enumCursor,
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-24', count: 0 },
    halted: null,
    ...over,
  };
}

beforeEach(() => {
  vi.stubGlobal('browser', withI18n({} as never));
});

describe('W113 · how a header key becomes a pair', () => {
  it('splits the platform from the scope at the first colon', () => {
    expect(splitHeaderKey(`${HEADER_KEY_PREFIX}chatgpt:default`)).toEqual({ platform: 'chatgpt', scope: 'default' });
    // 🔴 ADR-031's shape: the workspace id is a uuid and may itself contain colons.
    expect(splitHeaderKey(`${HEADER_KEY_PREFIX}chatgpt:ws:1234:abcd`)).toEqual({
      platform: 'chatgpt',
      scope: 'ws:1234:abcd',
    });
  });

  it('refuses anything that is not one of our keys with a scope in it', () => {
    for (const key of [
      'cs_backfill_v1:chatgpt:default',       // the pre-W18 layout: not a v2 header's address
      'cs_backfill_v2:',                      // no platform, no scope
      'cs_backfill_v2:chatgpt',               // no scope
      'cs_backfill_v2:chatgpt:',              // empty scope: not a pair
      'cs_hook_v1:chatgpt',                   // a different record entirely
      `cs_backfill_v2::default`,              // empty platform
    ]) {
      expect(splitHeaderKey(key), key).toBeNull();
    }
  });

  it('keyOfRow round-trips with stateKey', () => {
    expect(keyOfRow('claude', 'org-1')).toBe(stateKey('claude', 'org-1'));
    expect(splitHeaderKey(keyOfRow('claude', 'org-1'))).toEqual({ platform: 'claude', scope: 'org-1' });
  });
});

describe('W113 · which records the page will show', () => {
  it('accepts a header that agrees with the key it sits at', () => {
    expect(isReadableHeaderAt(stateKey('chatgpt', 'default'), header())).toBe(true);
  });

  it('accepts valid zero, nullable, optional, and colon-bearing boundary values', () => {
    const scope = 'ws:1234:abcd';
    expect(isReadableHeaderAt(stateKey('chatgpt', scope), header({
      scope,
      totalKnown: null,
      enumCursor: { offset: 0, complete: false, cursor: null, token: null },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '', count: 0 },
      lastFetchAt: { enumerate: null, detail: null },
    }))).toBe(true);
  });

  it('🔴 refuses a header whose own identity disagrees with its address', () => {
    // A set whose identity and address disagree is not one to show a user as their progress.
    expect(isReadableHeaderAt(stateKey('chatgpt', 'default'), header({ scope: 'other' }))).toBe(false);
    expect(isReadableHeaderAt(stateKey('chatgpt', 'default'), header({ platform: 'grok' }))).toBe(false);
  });

  it('🔴 refuses a value that is not a header at all, rather than reading it as an empty one', () => {
    for (const value of [null, undefined, 0, 'x', [], {}, { v: 2 }, { platform: 'chatgpt', scope: 'default', pending: ['a'] }]) {
      expect(isReadableHeaderAt(stateKey('chatgpt', 'default'), value), JSON.stringify(value)).toBe(false);
    }
  });

  it('🔴 refuses malformed nested header values instead of rendering missing counts or times as zero', () => {
    const malformed: Array<[string, Partial<BackfillHeader>]> = [
      ['string total counter', { totalKnown: '0' as unknown as number }],
      ['non-finite total counter', { totalKnown: Number.POSITIVE_INFINITY }],
      ['string pending counter', { pendingCount: '0' as unknown as number }],
      ['NaN pending counter', { pendingCount: Number.NaN }],
      ['infinite archived counter', { archivedCount: Number.POSITIVE_INFINITY }],
      ['negative archived counter', { archivedCount: -1 }],
      ['string counter inside daily object', { detailToday: { day: '2026-09-24', count: '0' as unknown as number } }],
      ['NaN daily counter', { detailToday: { day: '2026-09-24', count: Number.NaN } }],
      ['malformed daily counter object', { detailToday: null as unknown as BackfillHeader['detailToday'] }],
      ['wrong daily cap type', { detailToday: { day: '2026-09-24', count: 0, cap: '10' as unknown as number } }],
      ['NaN daily cap', { detailToday: { day: '2026-09-24', count: 0, cap: Number.NaN } }],
      ['malformed enum cursor object', { enumCursor: { offset: '0' as unknown as number, complete: false } }],
      ['non-finite enum cursor offset', { enumCursor: { offset: Number.POSITIVE_INFINITY, complete: false } }],
      ['missing enum cursor completion flag', { enumCursor: { offset: 0 } as unknown as BackfillHeader['enumCursor'] }],
      ['malformed last-fetch object', { lastFetchAt: { enumerate: Number.NaN, detail: null } }],
      ['non-finite detail timestamp', { lastFetchAt: { enumerate: null, detail: Number.NEGATIVE_INFINITY } }],
    ];

    for (const [label, change] of malformed) {
      expect(isReadableHeaderAt(stateKey('chatgpt', 'default'), header(change)), label).toBe(false);
    }
  });
});

describe('W113 · the target registry', () => {
  it('reads the rows, and drops anything that is not a target', () => {
    const rows = targetsOf({
      [BACKFILL_TARGETS_KEY]: [
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'default', at: 1 },
        { platform: 'chatgpt' },                       // no scope
        null,
        'nonsense',
        { scope: 'default' },                          // no platform
      ],
    });
    expect(rows.map((r) => `${r.platform}/${r.scope}`)).toEqual(['chatgpt/default']);
  });

  it('🔴 drops target rows with malformed fields or non-finite timestamps', () => {
    const rows = targetsOf({
      [BACKFILL_TARGETS_KEY]: [
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'default', at: 0 },
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'string-time', at: '1' },
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'nan-time', at: Number.NaN },
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'infinite-time', at: Number.POSITIVE_INFINITY },
        { platform: 'chatgpt', origin: 4, scope: 'wrong-origin-type', at: 1 },
        { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 4, at: 1 },
        { platform: 4, origin: 'https://chatgpt.com', scope: 'wrong-platform-type', at: 1 },
      ],
    });
    expect(rows.map((r) => `${r.platform}/${r.scope}`)).toEqual(['chatgpt/default']);
    expect(rows[0]?.at).toBe(0);
  });

  it('an absent or unreadable registry is an empty list, not a crash', () => {
    expect(targetsOf(null)).toEqual([]);
    expect(targetsOf({})).toEqual([]);
    expect(targetsOf({ [BACKFILL_TARGETS_KEY]: 'not an array' })).toEqual([]);
  });
});
