/**
 * W54b · **The eviction record is only honest if every surface of it holds.**
 *
 * tests/w54b-eviction-trace.test.ts pins the two facts the review required
 * through the registration path (which row the cap evicts; that the trace is
 * readable afterwards). This file pins the record's other three promises,
 * which the review's requirement (1) makes and nothing else would check:
 *
 *  · the **bound** is real: the receipt keeps `MAX_EVICTED_ENTRIES`, drops
 *    nothing silently, and says how many left (the C20 failures rule);
 *  · the **scope** is real: a registration with a free seat writes no
 *    receipt, and the W49b collapse's `non-organization` rows never enter it
 *    — a collapse is a deliberate replacement under the account model, and
 *    its scope can be a conversation title, which a surfaced record must not
 *    start storing;
 *  · the **surface** is real: a model carrying the record renders the note
 *    (and one carrying nothing renders nothing) — a record a later writer can
 *    silently drop from the render is not "surfaced where the popup reads
 *    registry state".
 *
 * 🔴 The suite pins the build channel to `dev` (`vitest.config.ts`); the
 *    channel test passes `'stable'` explicitly the way a shipped stable build
 *    would.
 */

import { describe, it, expect } from 'vitest';

import {
  evictionLogOf,
  MAX_EVICTED_ENTRIES,
  MAX_TARGET_ENTRIES,
  readEvictionLog,
  rememberOrganizationScopedTarget,
  rememberTarget,
  type BackfillTarget,
  type EvictionLog,
} from '../lib/backfill/alarm';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { popupText, renderPopup, NO_FAILURES, type PopupModel } from '../lib/popup-view';

const CHATGPT_ORIGIN = 'https://chatgpt.com';
const CLAUDE_ORIGIN = 'https://claude.ai';
const PERPLEXITY_ORIGIN = 'https://www.perplexity.ai';

const isOrgScope = (scope: string): boolean => /^org-[a-z]$/.test(scope);

function claudeRow(scope: string, at: number): BackfillTarget {
  return { platform: 'claude', origin: CLAUDE_ORIGIN, scope, at };
}

function chatgptRow(scope: string, at: number): BackfillTarget {
  return { platform: 'chatgpt', origin: CHATGPT_ORIGIN, scope, at };
}

/** The stored receipt, or null when nothing has been recorded. Not the popup's channel-filtered view. */
async function rawLog(store: BackfillStore): Promise<EvictionLog | null> {
  return readEvictionLog(await store.load('cs_backfill_evicted_v1'));
}

function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    ...overrides,
  };
}

describe('W54b · the eviction record\'s reader and its bound', () => {
  it('readEvictionLog refuses what it cannot read and invents nothing', () => {
    expect(readEvictionLog(null)).toBeNull();
    expect(readEvictionLog(undefined)).toBeNull();
    // A bare array is not the record this build writes, and neither is a
    // renamed field: reading either as "no evictions ever" would be a guess.
    expect(readEvictionLog([{ platform: 'claude', scope: 'org-a', reason: 'registry-cap', at: 1 }])).toBeNull();
    expect(readEvictionLog({ rows: [], dropped: 0 })).toBeNull();

    // One corrupt entry costs one row its place on the record, not the whole
    // record — the readTickCursor rule, applied to the receipt.
    const entry = { platform: 'claude', scope: 'org-a', reason: 'registry-cap', at: 5 };
    const mixed = readEvictionLog({
      entries: [entry, { platform: 'claude', scope: 7, reason: 'registry-cap', at: 5 }, 'garbage'],
      dropped: 2,
    });
    expect(mixed).toEqual({ entries: [entry], dropped: 2 });
    // A negative or non-integer counter is refused rather than clamped: the
    // count is this build's own fact about its own bound.
    expect(readEvictionLog({ entries: [], dropped: -1 })?.dropped).toBe(0);
    expect(readEvictionLog({ entries: [], dropped: 1.5 })?.dropped).toBe(0);
  });

  it('🔴 the receipt is a bounded window that says how many records its bound pushed off', async () => {
    const store = memoryStore();
    // Fill the registry, then keep registering fresh scopes: every
    // registration past the cap evicts exactly one seat. MAX_EVICTED_ENTRIES +
    // 2 more registrations make that many evictions, two more than the receipt
    // keeps: the two oldest must fall off it, and the count must say so.
    for (let i = 0; i < MAX_TARGET_ENTRIES; i += 1) {
      await rememberTarget(store, chatgptRow(`acct-${i}`, 100 + i));
    }
    for (let i = 0; i < MAX_EVICTED_ENTRIES + 2; i += 1) {
      await rememberTarget(store, chatgptRow(`overflow-${i}`, 1000 + i));
    }
    const log = await rawLog(store);
    expect(log).not.toBeNull();
    expect(log?.entries).toHaveLength(MAX_EVICTED_ENTRIES);
    expect(log?.dropped).toBe(2);
    // Newest first: the most recent eviction is the head, the same order the
    // registry itself keeps (most-recently-captured-first) — the 18th and
    // last eviction was the overflow-9 row (taken by registering
    // overflow-17), so the tail is the oldest eviction the receipt still
    // holds: acct-2, the third seat ever evicted.
    expect(log?.entries[0]?.scope).toBe('overflow-9');
    expect(log?.entries[0]?.reason).toBe('registry-cap');
    expect(log?.entries.at(-1)?.scope).toBe('acct-2');
  });

  it('🔴 a registration with a free seat writes no receipt', async () => {
    const store = memoryStore();
    await rememberTarget(store, chatgptRow('acct-only', 10));
    expect(await rawLog(store)).toBeNull();
  });

  it('🔴 the collapse\'s non-organization rows never enter the receipt — only the cap\'s do', async () => {
    const store = memoryStore();
    // The title row is only stored while Claude has no organization row
    // (W49/W31c), so it has to be registered first; the first organization
    // registration then collapses it — a write whose `dropped` is non-empty
    // with reason 'non-organization' and which evicts nothing.
    await rememberOrganizationScopedTarget(store, claudeRow('leftover-title', 40), isOrgScope);
    for (const name of ['a', 'b', 'c', 'd', 'e', 'f', 'g'].map((s) => `org-${s}`)) {
      await rememberOrganizationScopedTarget(store, claudeRow(name, 50), isOrgScope);
    }
    // 🔴 That write dropped a row and opened no receipt: the collapse is a
    //    deliberate replacement under the account model, and a title is
    //    conversation content a surfaced record must not start storing.
    expect(await rawLog(store), 'the collapse drop must not open the eviction receipt').toBeNull();

    // Fill to the cap, then one more: the receipt starts with the seat the
    // cap took, and only that seat.
    await rememberOrganizationScopedTarget(store, claudeRow('org-h', 90), isOrgScope);
    expect(await rawLog(store)).toBeNull();
    await rememberOrganizationScopedTarget(store, claudeRow('org-i', 95), isOrgScope);

    const log = await rawLog(store);
    expect(log).not.toBeNull();
    expect(log?.entries).toHaveLength(1);
    expect(log?.entries[0]?.platform).toBe('claude');
    expect(log?.entries[0]?.scope).toBe('org-a');
    expect(log?.entries[0]?.reason).toBe('registry-cap');
    expect(log?.dropped).toBe(0);
    expect(log?.entries.map((e) => e.scope)).not.toContain('leftover-title');
  });
});

describe('W54b · the popup\'s read of the record', () => {
  it('evictionLogOf keeps the note to the channel it belongs to', () => {
    // A dev build's eviction in a shared profile is that build's own news
    // (W91b): a stable read drops it and names nothing else either.
    const snapshot = {
      'cs_backfill_evicted_v1': {
        entries: [
          { platform: 'chatgpt', scope: 'acct-x', reason: 'registry-cap', at: 10 },
          { platform: 'perplexity', scope: 'dev-acct', reason: 'registry-cap', at: 11 },
        ],
        dropped: 0,
      },
    };
    const stable = evictionLogOf(snapshot, 'stable');
    expect(stable?.entries.map((e) => e.platform)).toEqual(['chatgpt']);
    // All rows another channel owns ⇒ no note for this one at all.
    const onlyDev = evictionLogOf({
      'cs_backfill_evicted_v1': {
        entries: [{ platform: 'perplexity', scope: 'dev-acct', reason: 'registry-cap', at: 11 }],
        dropped: 0,
      },
    }, 'stable');
    expect(onlyDev).toBeNull();
    // The dev channel still sees its own.
    expect(evictionLogOf({ 'cs_backfill_evicted_v1': { entries: [{ platform: 'perplexity', scope: 'dev-acct', reason: 'registry-cap', at: 11 }], dropped: 0 } }, 'dev')?.entries).toHaveLength(1);
    // Absent key, unreadable record, no snapshot: null, and nothing invented.
    expect(evictionLogOf({}, 'dev')).toBeNull();
    expect(evictionLogOf({ 'cs_backfill_evicted_v1': 'garbage' }, 'dev')).toBeNull();
    expect(evictionLogOf(null, 'dev')).toBeNull();
  });

  it('🔴 a model carrying the record renders the eviction note, with the reason in words and the stamp', () => {
    const AT = Date.parse('2026-09-24T10:00:00.000Z');
    const log: EvictionLog = {
      entries: [
        { platform: 'claude', scope: 'org-b', reason: 'registry-cap', at: AT },
        { platform: 'chatgpt', scope: 'acct-old', reason: 'registry-cap', at: AT },
      ],
      dropped: 3,
    };
    const view = renderPopup(model({ evictions: log }));
    const note = view.notes.join('\n');

    expect(note).toContain('Backfill target registry:');
    expect(note).toContain('2 eviction(s) are on record');
    expect(note).toContain('platform claude · archive scope org-b · evicted because the target registry was full · 2026-09-24 10:00:00 UTC');
    expect(note).toContain('platform chatgpt · archive scope acct-old');
    // The bound is named, and its own truncation is said out loud.
    expect(note).toContain('keeps at most 8 target(s)');
    expect(note).toContain('3 older eviction(s) are no longer on this record, which keeps the 16 most recent');
    // …and it reaches the flattened text the popup actually renders.
    expect(popupText(view)).toContain('evicted because the target registry was full');
  });

  it('🔴 an empty record renders no note — the popup does not invent an eviction', () => {
    const noLog = renderPopup(model({ evictions: null }));
    expect(noLog.notes.join('\n')).not.toContain('Backfill target registry:');
    const emptyEntries = renderPopup(model({ evictions: { entries: [], dropped: 0 } }));
    expect(emptyEntries.notes.join('\n')).not.toContain('Backfill target registry:');
    // The dropped sentence never appears alone: without entries there is no
    // note to attach it to.
    expect(emptyEntries.notes.join('\n')).not.toContain('older eviction(s)');
    // An absent field (older call sites) renders nothing either.
    expect(renderPopup(model()).notes.join('\n')).not.toContain('Backfill target registry:');
  });

  it('🔴 an unrecognised reason code prints as itself, never as a guess', () => {
    const view = renderPopup(model({
      evictions: { entries: [{ platform: 'claude', scope: 'org-f', reason: '-seat-lease-revoked', at: Date.parse('2026-09-24T10:00:00.000Z') }], dropped: 0 },
    }));
    expect(view.notes.join('\n')).toContain('evicted for an unrecognised reason: -seat-lease-revoked');
    expect(view.notes.join('\n')).not.toContain('evicted because the target registry was full');
  });
});
