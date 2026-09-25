/**
 * W54 · **The target registry's cap must not evict a row silently.**
 *
 * `rememberTarget` and `rememberOrganizationScopedTarget` both bound the active
 * registry with `MAX_TARGET_ENTRIES` (8) by slicing the tail off
 * `[target, ...rest]`. Before W54 a row pushed off that tail fell out of both
 * bookkeepings a write has: it never entered `dropped` (which only the
 * organization-scoped writer filled, and only with non-organization rows of the
 * platform being collapsed) and it was not in `stillPresent` (built from what
 * survived). So the registry row vanished while the scope's local ledger header
 * at `cs_backfill_v2:<platform>:<scope>` stayed behind — and since W49b the
 * popup hides a header whose scope is no longer a registered target, which left
 * the orphan invisible as well as uncleaned. The row that fell off could be an
 * organization, which is the same starvation D1 was written against, arriving
 * through a different door (W54, filed against W49b `71fbde2`).
 *
 * W54 makes every eviction a recorded drop, whatever the row kind: the reason
 * is `registry-cap` and the record comes from one pure seam (`capEvictions`), so
 * it cannot drift from the slice the cap performs. The row's local ledger
 * header is removed by the same rule as W49b's collapse (unless the identity
 * still holds a seat), so no orphaned, popup-hidden header can stay behind and
 * lie. A cap eviction is by-design cache pressure — like the collapse drop, it
 * carries no console warn; this module warns for faults.
 *
 * These tests pin the eviction path from the outside: the reason record itself,
 * the registry content, and the header keys in `storage.local`.
 *
 * 🔴 The suite pins the build channel to `dev` (`vitest.config.ts`); every
 *    platform used here is active in that channel, and the one channel test
 *    passes `'stable'` explicitly the way a shipped stable build would.
 */

import { describe, it, expect } from 'vitest';

import {
  BACKFILL_TARGETS_KEY,
  MAX_TARGET_ENTRIES,
  capEvictions,
  rememberOrganizationScopedTarget,
  rememberTarget,
  type BackfillTarget,
} from '../lib/backfill/alarm';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { headerOf, initialState, stateKey } from '../lib/backfill/types';

const CHATGPT_ORIGIN = 'https://chatgpt.com';
const CLAUDE_ORIGIN = 'https://claude.ai';
const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
const PERPLEXITY_ORIGIN = 'https://www.perplexity.ai';

/** The organization predicate a scoped platform's caller names. UUID-shaped here. */
const isOrgScope = (scope: string): boolean => /^org-[a-z]$/.test(scope);

function claudeRow(scope: string, at: number): BackfillTarget {
  return { platform: 'claude', origin: CLAUDE_ORIGIN, scope, at };
}

function chatgptRow(scope: string, at: number): BackfillTarget {
  return { platform: 'chatgpt', origin: CHATGPT_ORIGIN, scope, at };
}

function deepseekRow(scope: string, at: number): BackfillTarget {
  return { platform: 'deepseek', origin: DEEPSEEK_ORIGIN, scope, at };
}

/** The evidence W54 is about: a genuine local ledger header under `cs_backfill_v2:*`. */
async function writeHeader(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<void> {
  const state = initialState(platform, scope);
  state.archived = ['synthetic-archived'];
  await store.save(stateKey(platform, scope), headerOf(state));
}

/** `memoryStore.load` answers `null` for a key nothing holds anymore. */
async function headerGone(
  store: BackfillStore,
  platform: string,
  scope: string,
): Promise<boolean> {
  return (await store.load(stateKey(platform, scope))) === null;
}

async function registryRows(store: BackfillStore): Promise<BackfillTarget[]> {
  const raw = await store.load(BACKFILL_TARGETS_KEY);
  return Array.isArray(raw) ? (raw as BackfillTarget[]) : [];
}

describe('W54-1 · the plain registry writer\'s cap eviction', () => {
  it('🔴 a full registry evicts its tail row, and that scope\'s local ledger header goes with it', async () => {
    const store = memoryStore();
    // Newest first, as a real registry accumulates: the tail is the stalest seat.
    const rows = Array.from({ length: MAX_TARGET_ENTRIES }, (_, i) =>
      chatgptRow(`acct-${i}`, MAX_TARGET_ENTRIES - i));
    await store.save(BACKFILL_TARGETS_KEY, rows);
    for (const row of rows) await writeHeader(store, row.platform, row.scope);

    const returned = await rememberTarget(store, chatgptRow('acct-fresh', 99));

    // The cap itself is old behaviour: eight in, the tail row out.
    expect(returned).toHaveLength(MAX_TARGET_ENTRIES);
    const written = await registryRows(store);
    expect(written).toHaveLength(MAX_TARGET_ENTRIES);
    expect(written.some((t) => t.scope === 'acct-7')).toBe(false);
    expect(written.some((t) => t.scope === 'acct-fresh')).toBe(true);

    // 🔴 The eviction is recorded: the evicted scope's header must not
    //    outlive its registry row (on main it stays behind, invisible).
    expect(await headerGone(store, 'chatgpt', 'acct-7')).toBe(true);
    // …and no kept row lost its header.
    for (let i = 0; i < MAX_TARGET_ENTRIES - 1; i += 1) {
      expect(await headerGone(store, 'chatgpt', `acct-${i}`)).toBe(false);
    }
  });

  it('🔴 the eviction record is `reason: registry-cap` for every tail row, and nothing within the cap is a drop', () => {
    const withinCap = Array.from({ length: MAX_TARGET_ENTRIES }, (_, i) =>
      chatgptRow(`acct-${i}`, MAX_TARGET_ENTRIES - i));
    expect(capEvictions(withinCap), 'exactly at the cap: no row evicted, none recorded').toEqual([]);

    const withIncoming = [chatgptRow('acct-fresh', 99), ...withinCap];
    const drops = capEvictions(withIncoming);
    expect(drops).toHaveLength(1);
    expect(drops.map((d) => d.reason)).toEqual(['registry-cap']);
    expect(drops[0]?.target.scope).toBe(`acct-${MAX_TARGET_ENTRIES - 1}`);

    // More than one row past the cap: every one of them is a recorded drop.
    const twoPast = [chatgptRow('acct-first', 99), chatgptRow('acct-second', 98), ...withinCap];
    const twoDrops = capEvictions(twoPast);
    expect(twoDrops).toHaveLength(2);
    expect(twoDrops.every((d) => d.reason === 'registry-cap')).toBe(true);
    expect(twoDrops.map((d) => d.target.scope)).toEqual([
      `acct-${MAX_TARGET_ENTRIES - 2}`, `acct-${MAX_TARGET_ENTRIES - 1}`,
    ]);

    // Whatever the row kind: an organization row falls off the cap as the
    // same recorded reason, not as the collapse's 'non-organization'.
    const orgDrops = capEvictions(withIncoming.map((t, i) =>
      i === MAX_TARGET_ENTRIES ? claudeRow('org-tail', 1) : t));
    expect(orgDrops.map((d) => d.reason)).toEqual(['registry-cap']);
    expect(orgDrops[0]?.target.platform).toBe('claude');
  });

  it('🔴 an evicted duplicate does not erase the local ledger of the identity that survived', async () => {
    const store = memoryStore();
    // Nine stored active rows — one more than the cap — with `acct-a`'s identity
    // carried by two of them. Stored storage can hold the duplicate; the-newer
    // copy survives while the older one falls off, and the header belongs to
    // the identity, not to either copy.
    const rows = [
      chatgptRow('acct-a', 9),
      ...['b', 'c', 'd', 'e', 'f', 'g', 'h'].map((s, i) => chatgptRow(`acct-${s}`, 8 - i)),
      chatgptRow('acct-a', 0),
    ];
    await store.save(BACKFILL_TARGETS_KEY, rows);
    for (const row of rows) await writeHeader(store, row.platform, row.scope);

    await rememberTarget(store, chatgptRow('acct-fresh', 99));

    const written = await registryRows(store);
    expect(written).toHaveLength(MAX_TARGET_ENTRIES);
    // `acct-h` had one copy, it fell off, and its header must go with it.
    expect(written.some((t) => t.scope === 'acct-h')).toBe(false);
    expect(await headerGone(store, 'chatgpt', 'acct-h')).toBe(true);
    // `acct-a`'s surviving copy keeps the seat, so the identity keeps its header.
    expect(written.filter((t) => t.scope === 'acct-a')).toHaveLength(1);
    expect(await headerGone(store, 'chatgpt', 'acct-a')).toBe(false);
  });
});

describe('W54-2 · the organization-scoped writer\'s cap eviction', () => {
  it('🔴 an organization pushed off the cap loses its local ledger header — never a silent eviction', async () => {
    const store = memoryStore();
    const orgs = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']
      .map((s, i) => claudeRow(`org-${s}`, 20 - i));
    await store.save(BACKFILL_TARGETS_KEY, orgs);
    for (const row of orgs) await writeHeader(store, row.platform, row.scope);

    const returned = await rememberOrganizationScopedTarget(
      store, claudeRow('org-i', 99), isOrgScope,
    );

    // The cap is old behaviour: eight organizations in, the tail one out.
    expect(returned).toHaveLength(MAX_TARGET_ENTRIES);
    const written = await registryRows(store);
    expect(written.some((t) => t.scope === 'org-h')).toBe(false);
    expect(written.some((t) => t.scope === 'org-i')).toBe(true);

    // 🔴 On main the evicted organization's header stays behind, orphaned and
    //    (since W49b) invisible — the exact evidence W54 is about.
    expect(await headerGone(store, 'claude', 'org-h')).toBe(true);
    for (const row of orgs.slice(0, MAX_TARGET_ENTRIES - 1)) {
      expect(await headerGone(store, row.platform, row.scope)).toBe(false);
    }
  });

  it('🔴 a non-organization row of another platform falls off the cap the same way, whatever the row kind', async () => {
    const store = memoryStore();
    // Not a collapse: the chatgpt row is not a non-organization row *of the
    // platform being written*, so on main it is evicted by the cap with no
    // drop record at all — the row-kind hole W54 names.
    const orgs = ['a', 'b', 'c', 'd', 'e', 'f', 'g'].map((s, i) => claudeRow(`org-${s}`, 20 - i));
    const otherPlatformRow = chatgptRow('acct-x', 1);
    await store.save(BACKFILL_TARGETS_KEY, [...orgs, otherPlatformRow]);
    for (const row of [...orgs, otherPlatformRow]) {
      await writeHeader(store, row.platform, row.scope);
    }

    await rememberOrganizationScopedTarget(store, claudeRow('org-i', 99), isOrgScope);

    const written = await registryRows(store);
    expect(written).toHaveLength(MAX_TARGET_ENTRIES);
    expect(written.some((t) => t.platform === 'chatgpt')).toBe(false);
    expect(await headerGone(store, 'chatgpt', 'acct-x')).toBe(true);
  });
});

describe('W54-3 · what the eviction record must not break', () => {
  it('two organizations still coexist under the cap, and re-capturing one evicts nothing', async () => {
    const store = memoryStore();

    await rememberOrganizationScopedTarget(store, claudeRow('org-a', 10), isOrgScope);
    await rememberOrganizationScopedTarget(store, claudeRow('org-b', 11), isOrgScope);

    let written = await registryRows(store);
    expect(written.map((t) => t.scope).sort()).toEqual(['org-a', 'org-b']);

    // A re-capture moves the row to the front; it evicts no row and drops none.
    await rememberOrganizationScopedTarget(store, claudeRow('org-a', 12), isOrgScope);
    written = await registryRows(store);
    expect(written.map((t) => t.scope).sort()).toEqual(['org-a', 'org-b']);
  });

  it('D1 and D4 guards: a title collapse inserts no sentinel, and the collapsed row loses its header', async () => {
    const store = memoryStore();
    await store.save(BACKFILL_TARGETS_KEY, [
      claudeRow('org-a', 9),
      claudeRow('read-me-title', 8),
    ]);
    await writeHeader(store, 'claude', 'org-a');
    await writeHeader(store, 'claude', 'read-me-title');

    // Collapsing another title while a live organization exists: D1 keeps the
    // org at the head (no 'default' in front of it) and nothing is evicted.
    await rememberOrganizationScopedTarget(store, claudeRow('one-more-title', 7), isOrgScope);

    const written = await registryRows(store);
    expect(written).toHaveLength(1);
    expect(written[0]?.scope).toBe('org-a');
    // D4, unchanged: the collapsed non-organization row's header is removed.
    expect(await headerGone(store, 'claude', 'read-me-title')).toBe(true);
    expect(await headerGone(store, 'claude', 'org-a')).toBe(false);
  });

  it('a stable write\'s eviction never touches the dev channel\'s leftover row or its header', async () => {
    const store = memoryStore();
    const leftover = { platform: 'perplexity', origin: PERPLEXITY_ORIGIN, scope: 'dev-acct', at: 1 };
    const fullStable = Array.from({ length: MAX_TARGET_ENTRIES }, (_, i) =>
      (i % 2 === 0 ? deepseekRow(`s${i}`, i + 1) : chatgptRow(`s${i}`, i + 1)));
    await store.save(BACKFILL_TARGETS_KEY, [leftover, ...fullStable]);
    for (const row of [...fullStable, leftover]) {
      await writeHeader(store, row.platform, row.scope);
    }

    await rememberTarget(store, chatgptRow('brand-new', 99), 'stable');

    const written = await registryRows(store);
    expect(written).toHaveLength(MAX_TARGET_ENTRIES + 1);
    expect(JSON.stringify(written.some((t) =>
      t.platform === 'perplexity' && t.scope === 'dev-acct'))).toBe('true');
    // W91b, on the eviction path too: the leftover row neither consumes a
    // stable slot nor loses its header to a stable write's cap.
    expect(await headerGone(store, 'perplexity', 'dev-acct')).toBe(false);
    // The stable row that fell off loses its header like any eviction.
    expect(await headerGone(store, 'chatgpt', 's7')).toBe(true);
  });
});
