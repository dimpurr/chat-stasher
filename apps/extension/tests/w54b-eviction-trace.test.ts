/**
 * W54b · **An eviction from the target registry must be traceable, from the
 * storage a later reader can open — not just from a value the writer computed
 * and threw away.**
 *
 * ## What the review found (verbatim target of this file)
 *
 * The W54 fix made a cap eviction remove the evicted scope's local ledger
 * header, but the record of *why the row left* still lived only in the writer's
 * local `dropped` array, which was discarded the moment the write returned.
 * After that, nothing observable names the evicted target: the registry no
 * longer holds it, its header is gone (so the popup, which since W49b hides
 * headers whose scope is not registered, shows nothing), and the trace record
 * does not exist. The row left without a trace — which is exactly the "never
 * evict an organization row silently" the W54 task text asked for.
 *
 * ## What the two tests here pin
 *
 *  · the registry is built **through the real registration path** — one
 *    `rememberTarget` / `rememberOrganizationScopedTarget` call per row, never a
 *    seeded `store.save(BACKFILL_TARGETS_KEY, …)` — because seeding tests the
 *    slice, not the writer, and the review was about what the writer leaves
 *    behind;
 *  · an **older target is re-touched** before the eviction, so the seat that
 *    falls off is the least recently registered one. Ordering is the whole
 *    question W54 left untested: the re-touched row must survive and the row
 *    that waited longest must be the one that leaves;
 *  · the test names **exactly which row** was evicted — in the registry's
 *    content, in `returned`, and in the trace;
 *  · the trace is read back from storage (`store.load('…evicted…')`) after the
 *    writer has returned, so what is asserted is a durable record a later
 *    reader (the popup) can open, not a value still living in this test.
 *
 * 🔴 The eviction record's **key is written as a literal** (`cs_backfill_evicted_v1`)
 *    on purpose. Importing the constant would tie this file to the fixed build;
 *    as a literal the same file compiles on the code *without* the W54b fix —
 *    on this branch before the fix, and on `main` — and fails there as real red
 *    assertions ("expected the record to hold the evicted scope, got none"),
 *    which is the evidence a regression test must be able to produce. It also
 *    pins the persisted key name, the same thing docs-dev/privacy.md documents.
 *
 * 🔴 The suite pins the build channel to `dev` (`vitest.config.ts`); every
 *    platform used here is active in that channel.
 */

import { describe, it, expect } from 'vitest';

import {
  BACKFILL_TARGETS_KEY,
  MAX_TARGET_ENTRIES,
  rememberOrganizationScopedTarget,
  rememberTarget,
  type BackfillTarget,
} from '../lib/backfill/alarm';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { headerOf, initialState, stateKey } from '../lib/backfill/types';

const CHATGPT_ORIGIN = 'https://chatgpt.com';
const CLAUDE_ORIGIN = 'https://claude.ai';

/** The organization predicate a scoped platform's caller names. UUID-shaped here. */
const isOrgScope = (scope: string): boolean => /^org-[a-z]$/.test(scope);

function claudeRow(scope: string, at: number): BackfillTarget {
  return { platform: 'claude', origin: CLAUDE_ORIGIN, scope, at };
}

function chatgptRow(scope: string, at: number): BackfillTarget {
  return { platform: 'chatgpt', origin: CHATGPT_ORIGIN, scope, at };
}

/**
 * The evidence W54/W54b are about: a genuine local ledger header under
 * `cs_backfill_v2:*`, which an eviction must take with the row.
 */
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

/**
 * The durable eviction record W54b adds. Written as the bare shape check it is:
 * this file must not import the fixed build's reader, so it validates the
 * persisted record itself — which is also the honest way to pin what storage
 * promises to whatever reads it next.
 */
function evictedEntries(raw: unknown): Array<Record<string, unknown>> {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return [];
  const entries = (raw as { entries?: unknown }).entries;
  if (!Array.isArray(entries)) return [];
  return entries.filter(
    (e): e is Record<string, unknown> => Boolean(e && typeof e === 'object' && !Array.isArray(e)),
  );
}

describe('W54b · a registration-path eviction leaves a trace storage can still name', () => {
  it('🔴 the plain writer: the least recently registered seat leaves, and the record says so — readable afterwards', async () => {
    const store = memoryStore();
    // Build the FULL registry through the real registration path, one row per
    // call, increasing `at` so the first row registered is also the row that
    // has waited longest. Seeding the key directly would prove the slice, not
    // this writer; W54's own tests already covered that seam.
    for (let i = 0; i < MAX_TARGET_ENTRIES; i += 1) {
      await rememberTarget(store, chatgptRow(`acct-${i}`, 100 + i));
    }
    // Header evidence for the two rows this test is about: the seat about to
    // fall off, and the seat that must survive it.
    await writeHeader(store, 'chatgpt', 'acct-0');
    await writeHeader(store, 'chatgpt', 'acct-1');

    // 🔴 Re-touch an older target. `acct-0` was registered first and is the
    //    stalest seat, but a real capture re-registers it now, so it moves to
    //    the head — and the NEXT eviction must take `acct-1`, the row that has
    //    waited longest since. This ordering is what the review said the old
    //    tests could not prove.
    await rememberTarget(store, chatgptRow('acct-0', 4000));
    // A wall-clock read taken *before* the evicting write, minus a wide margin
    // for a clock that steps backwards mid-test: the stamp must be the
    // registration's own moment, not the row's last-seen time.
    const evictionAt = Date.now() - 60_000;
    const returned = await rememberTarget(store, chatgptRow('acct-fresh', 5000));

    // Exactly which row left: `acct-1` — and nothing else.
    const written = await registryRows(store);
    expect(written).toHaveLength(MAX_TARGET_ENTRIES);
    expect(written.some((t) => t.platform === 'chatgpt' && t.scope === 'acct-1')).toBe(false);
    expect(returned.some((t) => t.platform === 'chatgpt' && t.scope === 'acct-fresh')).toBe(true);
    // 🔴 LRU: the re-touched row keeps its seat even though it was registered
    //    first; the evicted seat is the one that waited longest.
    expect(written.some((t) => t.platform === 'chatgpt' && t.scope === 'acct-0')).toBe(true);

    // The evicted scope's header left with the row (W54's own half; on main
    // this line is already red — the header used to stay behind, orphaned).
    expect(await headerGone(store, 'chatgpt', 'acct-1')).toBe(true);
    // …and the re-touched row kept its header, because its identity kept its seat.
    expect(await headerGone(store, 'chatgpt', 'acct-0')).toBe(false);

    // 🔴 W54b · The trace, read back from storage AFTER the writer has
    //    returned. On main and on the pre-W54b branch this read finds nothing
    //    at all — the record was a local array the writer threw away.
    const raw = await store.load('cs_backfill_evicted_v1');
    const record = evictedEntries(raw);
    expect(record, 'the eviction must leave a record behind').toHaveLength(1);
    const entry = record[0]!;
    expect(entry.platform).toBe('chatgpt');
    expect(entry.scope).toBe('acct-1');
    expect(entry.reason).toBe('registry-cap');
    // `at` is the moment of the registration that evicted the row — a real
    // stamp, not the row's own last-seen time.
    const at = entry.at;
    expect(typeof at).toBe('number');
    expect(at as number).toBeGreaterThanOrEqual(evictionAt);
  });

  it('🔴 the organization-scoped writer: an organization losing the LRU seat is nameable afterwards, never silent', async () => {
    const store = memoryStore();
    // The full registry again, this time eight organizations through the real
    // organization-scoped writer — the writer whose collapse rule (W49/W49b)
    // makes `dropped` also carry non-organization rows, so the cap's rows and
    // the collapse's rows must stay distinguishable on the record.
    for (let i = 0; i < MAX_TARGET_ENTRIES; i += 1) {
      const name = `${String.fromCharCode('a'.charCodeAt(0) + i)}`;
      await rememberOrganizationScopedTarget(store, claudeRow(`org-${name}`, 100 + i), isOrgScope);
    }
    await writeHeader(store, 'claude', 'org-a');
    await writeHeader(store, 'claude', 'org-b');

    // 🔴 Re-touch the oldest organization: `org-a` was registered first, but a
    //    capture now re-registers it to the head, so the seat that falls off is
    //    `org-b` — the organization that has waited longest.
    await rememberOrganizationScopedTarget(store, claudeRow('org-a', 4000), isOrgScope);
    const evictionAt = Date.now() - 60_000;
    const returned = await rememberOrganizationScopedTarget(store, claudeRow('org-i', 5000), isOrgScope);

    // Exactly which organization left.
    const written = await registryRows(store);
    expect(written).toHaveLength(MAX_TARGET_ENTRIES);
    expect(written.some((t) => t.platform === 'claude' && t.scope === 'org-b')).toBe(false);
    expect(returned.some((t) => t.platform === 'claude' && t.scope === 'org-i')).toBe(true);
    expect(written.some((t) => t.platform === 'claude' && t.scope === 'org-a')).toBe(true);

    // W54's half: the evicted organization's header goes with it (red on
    // main), and the re-touched one keeps its own.
    expect(await headerGone(store, 'claude', 'org-b')).toBe(true);
    expect(await headerGone(store, 'claude', 'org-a')).toBe(false);

    // 🔴 W54b · The organization's eviction is on the record, with the reason
    //    it left — readable from storage once the writer is done. Silent is the
    //    one thing an organization losing its backfill seat must never be.
    const raw = await store.load('cs_backfill_evicted_v1');
    const record = evictedEntries(raw);
    expect(record, 'the organizational eviction must leave a record behind').toHaveLength(1);
    const entry = record[0]!;
    expect(entry.platform).toBe('claude');
    expect(entry.scope).toBe('org-b');
    expect(entry.reason).toBe('registry-cap');
    const at = entry.at;
    expect(typeof at).toBe('number');
    expect(at as number).toBeGreaterThanOrEqual(evictionAt);
  });
});
