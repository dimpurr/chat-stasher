/**
 * W53 · **A stale hook failure must be retractable — by age, and only by age.**
 *
 * ## What this file is protecting
 *
 * The one observation that writes a hook record is the *top frame* of that
 * origin, and the one observation that clears it is a *later top frame whose hook
 * verified* (`sendHookStatus`, `entrypoints/dw-bridge.content.ts`, returns at the
 * top-frame gate). Both gates are intentional and must not move (W46).
 *
 * The hole, measured in W53: a top document half-installs and writes
 * `hook-was-replaced`, that tab closes, and the origin thereafter appears only as
 * a **nested frame**. The child's hook is healthy and verifies, but the bridge
 * drops its observation — it is not the top frame — so `store.remove` never runs.
 * Nothing else ever will: the popup's "reloading the tab" remedy reloads the
 * *host* page of the embedder, not a top document on that origin. The record
 * stops being re-reported the moment the last top document on it closes, and
 * stays in storage forever, and the popup keeps asserting a page-state that is no
 * longer being asserted.
 *
 * That is invariant 1 pointing the *other* way from the usual one: a warning that
 * has stopped being true. W46 traded "wrongly cleared by a sibling frame" for
 * "possibly never cleared". The trade was right — a false all-clear is worse than
 * a stale warning — but the stale warning is still a record that has stopped
 * being a record of anything. The retraction must be **age**, because age is the
 * only signal that survives both gates:
 *
 *   · a page still in the state it reported re-reports it on a timer
 *     (`HOOK_SELF_CHECK_INTERVAL_MS`, `lib/page-hook.ts`) and "a record
 *     re-reported within the window" keeps the state current;
 *   · a page that is gone, or an origin that is now only a subframe, stops
 *     re-reporting, so "a record not re-reported within the window" is the fact
 *     that the state is no longer being asserted.
 *
 * ## The perimeter, unchanged
 *
 * Age-out is the **only** new way a record leaves storage. It must not let a
 * child frame clear the record (the child has nothing to do with it), it must
 * keep `remove` — an emptied *row* is not the same storage fact as `remove` and
 * this file asserts the difference — and it must not roll back the top-frame
 * gate. The record and its retraction live in one instance's StorageArea, so
 * nothing an instance does here can clear another instance's record.
 *
 * ## What is asserted
 *
 *  1. The window is larger than the failure re-report cadence, so a genuinely
 *     broken page that is *still open* is never aged out (its `at` keeps moving).
 *  2. A record whose current observation has stopped being re-reported is
 *     `isHookStatusStale`.
 *  3. `pruneStaleHookStatus` removes the stale one with `store.remove` — the key
 *     is gone, not rewritten as an empty record — and leaves the fresh one.
 */

import { describe, expect, it } from 'vitest';
import { HOOK_REASON_WAS_REPLACED } from '../lib/contract';
import {
  HOOK_STATUS_STALE_AFTER_MS,
  hookStatusKey,
  isHookStatusStale,
  pruneStaleHookStatus,
  type HookStatusRecord,
} from '../lib/hook-status';
import type { BackfillStore } from '../lib/backfill/store';

/** A store with the port's four members and nothing else, like `browserLocalStore`'s. */
function memoryStore(initial: Record<string, unknown> = {}) {
  const rows = new Map<string, unknown>(Object.entries(initial));
  const store: BackfillStore = {
    async load(key) { return rows.has(key) ? rows.get(key) : null; },
    async save(key, value) { rows.set(key, value); },
    async remove(key) { rows.delete(key); },
    async keys() { return [...rows.keys()]; },
  };
  return { store, rows };
}

function record(overrides: Partial<HookStatusRecord> = {}): HookStatusRecord {
  const at = overrides.at ?? 5_000;
  return {
    origin: 'https://chatgpt.com',
    platform: 'chatgpt',
    reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at, since: at }],
    at,
    ...overrides,
  };
}

const NOW = 60_000;

describe('W53 · a stale hook failure is retracted by age, and only by that', () => {
  it('the staleness window is strictly larger than the re-report cadence', () => {
    // The re-report cadence is HOOK_SELF_CHECK_INTERVAL_MS = 5s; the window must
    // clear a throttled background tab (≈1 min between checks) without ever
    // reaching a page whose failure is still being re-reported.
    expect(HOOK_STATUS_STALE_AFTER_MS).toBeGreaterThan(60_000);
  });

  it('is false for a record the origin still re-reports', () => {
    const fresh = record({ at: NOW - 1_000 }); // reported a second ago
    expect(isHookStatusStale(fresh, NOW)).toBe(false);
  });

  it('is true once the record stops being re-reported for the window', () => {
    const stale = record({ at: NOW - HOOK_STATUS_STALE_AFTER_MS - 1 });
    expect(isHookStatusStale(stale, NOW)).toBe(true);
  });

  it('a record stamped in the future (a clock that moved) is never aged out', () => {
    const future = record({ at: NOW + 10_000 });
    expect(isHookStatusStale(future, NOW)).toBe(false);
  });

  it('prune removes a stale record with remove, not by writing an empty row', async () => {
    const stale = record({ origin: 'https://groq.com', platform: 'grok', at: NOW - HOOK_STATUS_STALE_AFTER_MS - 1 });
    const fresh = record({ at: NOW - 1_000 });
    const staleKey = hookStatusKey(stale.origin);
    const freshKey = hookStatusKey(fresh.origin);
    const { store, rows } = memoryStore({ [staleKey]: stale, [freshKey]: fresh });

    const kept = await pruneStaleHookStatus(store, Object.fromEntries(rows), NOW, 'dev');

    expect(rows.has(staleKey)).toBe(false); // removed, not saved-over
    expect(rows.has(freshKey)).toBe(true);
    expect(kept.map((r) => r.origin)).toEqual([fresh.origin]);
  });

  it('prune leaves a fresh record in storage untouched', async () => {
    const fresh = record({ at: NOW - 1_000 });
    const key = hookStatusKey(fresh.origin);
    const { store, rows } = memoryStore({ [key]: fresh });

    const kept = await pruneStaleHookStatus(store, Object.fromEntries(rows), NOW, 'dev');

    expect(rows.has(key)).toBe(true);
    expect(kept).toEqual([fresh]);
  });

  it('prune over the W53 scenario retracts it and leaves nothing behind', async () => {
    // The exact hole: a top frame wrote hook-was-replaced, the tab closed, the
    // origin now appears only as a nested frame, so nothing re-reports it.
    const stale = record({ at: NOW - HOOK_STATUS_STALE_AFTER_MS - 1 });
    const { store, rows } = memoryStore({ [hookStatusKey(stale.origin)]: stale });

    const kept = await pruneStaleHookStatus(store, Object.fromEntries(rows), NOW, 'dev');

    expect(kept).toEqual([]);
    expect(rows.size).toBe(0); // nothing left behind, and no empty row
  });

  it('prune with no store still retracts the stale record from what is shown', async () => {
    const stale = record({ at: NOW - HOOK_STATUS_STALE_AFTER_MS - 1 });
    const snapshot = { [hookStatusKey(stale.origin)]: stale };
    // A popup whose StorageArea cannot be reached must still not show the stale
    // claim; only the physical removal is skipped.
    const kept = await pruneStaleHookStatus(null, snapshot, NOW, 'dev');
    expect(kept).toEqual([]);
  });
});

/**
 * W91b mirror · **A stable prune must not delete or surface a dev build's
 * leftover experimental record.**
 *
 * W53's retraction is channel-agnostic here: `pruneStaleHookStatus` walks every
 * `hookStatusOf` record and removes whatever is stale, which means a *stable*
 * popup open — sharing one extension ID and one `storage.local` with the dev
 * build via the manifest `key` — deletes the experimental record a dev build is
 * still the only witness of. W91 already guarantees the stable popup never
 * *surfaces* it; W91b pins it never *deletes* it either. Only the channel that
 * serves the platform may age it out, and the stable build simply has nothing to
 * say about experimental state.
 */
describe('W53/W91b · prune is channel-scoped', () => {
  it('🔴 a stable build must not delete or surface a dev build\x27s leftover', async () => {
    const PERPLEXITY_ORIGIN = 'https://www.perplexity.ai';
    // A dev build wrote this while the experimental page was open; it is stale
    // (>5 min, the page swapped out), but only the dev build can say it stopped.
    const devLeftover = record({
      origin: PERPLEXITY_ORIGIN,
      platform: 'perplexity',
      at: NOW - HOOK_STATUS_STALE_AFTER_MS - 1,
    });
    const stableStale = record({
      origin: 'https://chat.deepseek.com',
      platform: 'deepseek',
      at: NOW - HOOK_STATUS_STALE_AFTER_MS - 1,
    });
    const devKey = hookStatusKey(devLeftover.origin);
    const stableKey = hookStatusKey(stableStale.origin);
    const snapshot = { [devKey]: devLeftover, [stableKey]: stableStale };
    const { store, rows } = memoryStore(snapshot);

    // A stable build prunes its own channel: the stale stable record is removed
    // by age, but the experimental record must survive in storage byte-for-byte.
    const kept = await pruneStaleHookStatus(store, snapshot, NOW, 'stable');

    expect(rows.has(stableKey)).toBe(false); // stable ages out its own stale
    expect(rows.has(devKey)).toBe(true); // ...and never the dev build's leftover
    expect(kept.some((r) => r.platform === 'perplexity')).toBe(false); // and never surfaces it

    // Only the channel that serves the platform may age it out: a dev build —
    // this record's owner — still prunes it by age, exactly as W53 intends.
    const devKept = await pruneStaleHookStatus(store, snapshot, NOW, 'dev');
    expect(rows.has(devKey)).toBe(false); // the owning channel CAN retract it
    expect(devKept).toEqual([]);
  });
});