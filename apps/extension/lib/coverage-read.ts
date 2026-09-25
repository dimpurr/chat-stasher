/**
 * ADR-032 · **The reading half of the coverage page: this browser's own storage, and nothing else.**
 *
 * `lib/coverage.ts` is the model and reads nothing; this is what fills it in. The split is what makes "the
 * page never contacts a chat platform" checkable: everything that could reach outside this extension lives
 * on this side of the line, and there is nothing here that reaches outside it either — `storage.local`, the
 * backfill IndexedDB, and the host-pause record. No port, no tab, no `fetch`, no Native Messaging call.
 *
 * 🔴 **Why the page does not ask the host for the archive's own counts.** ADR-028 §1 made the CLI's `ui`
 *    the archive's truth and ADR-032 §2 kept that division. Reading the archive from here would mean a new
 *    Native Messaging request shape — a protocol change this ticket is not authorised to make — and it
 *    would put a *second* copy of the archive's numbers in front of the user, which is exactly the
 *    duplication ADR-028 exists to avoid. The page says what it knows and says which half it is.
 *
 * ## What it reads, and what each source is for
 *
 * | Source | Key / store | Why |
 * |---|---|---|
 * | progress headers | `storage.local` `cs_backfill_v2:<platform>:<scope>` | one row per platform+account: cursor, totals, counters, failures, the stop |
 * | debt rows | IndexedDB `chat-stasher-backfill` / `debts_by_platform` | **the authority** on what is owed vs settled (`lib/backfill/ledger.ts`) |
 * | debt times | the same store, row field `at` (W113) | the platform's own time for a conversation, when its list gave one |
 * | targets | `storage.local` `cs_backfill_targets_v1` | distinguishes "nothing owed" from "no target, so nothing runs" |
 * | switch | `storage.local` `cs_backfill_enabled_v1` | off means nothing runs, whatever else is on disk |
 * | host pause | `storage.local` `cs_native_host_pause_v1` | the delivery exit is unreachable |
 * | tick trace | `storage.local` `cs_backfill_lasttick_v1` | which platform the last wake passed over, and why |
 *
 * Everything is **read-only**: the only write anywhere near this page is the speed preset, and it is not in
 * this module.
 */

import { BACKFILL_TARGETS_KEY, loadLastTick, type BackfillTarget, type BackfillTickRecord, type TickSkipReason } from './backfill/alarm';
import { readDebtSet } from './backfill/debt-store';
import { isBackfillEnabled } from './backfill/schedule';
import { readSpeedPreset } from './backfill/speed';
import { browserLocalSnapshot, type BackfillStore } from './backfill/store';
import {
  BACKFILL_STATE_VERSION,
  isHeader,
  stateKey,
  type BackfillHeader,
} from './backfill/types';
import { currentReleaseChannel, isPlatformActiveInChannel } from './contract';
import { loadHostPause } from './host-status';
import type { CoverageDebtInput, CoverageScopeInput, CoverageInput } from './coverage';

/** The key prefix every progress header is written at. Derived, so a version bump cannot leave this behind. */
export const HEADER_KEY_PREFIX = `cs_backfill_v${BACKFILL_STATE_VERSION}:`;

/**
 * One `cs_backfill_v2:<platform>:<scope>` key split into its two halves.
 *
 * 🔴 The platform is one segment and the scope is **everything after it**, for the reason
 *    `lib/backfill/ledger.ts`'s `ownersOfScope` gives: a scope is an account or organization identifier and
 *    may itself contain a colon (ADR-031's `chatgpt:<workspace-id>` is exactly that shape). Splitting on
 *    the *last* colon would silently truncate such a scope, and the truncated string would then be used as
 *    a map key — a wrong answer with no error.
 */
export function splitHeaderKey(key: string): { platform: string; scope: string } | null {
  if (!key.startsWith(HEADER_KEY_PREFIX)) return null;
  const rest = key.slice(HEADER_KEY_PREFIX.length);
  const separator = rest.indexOf(':');
  if (separator < 1) return null;
  const scope = rest.slice(separator + 1);
  if (scope.length === 0) return null;
  return { platform: rest.slice(0, separator), scope };
}

/** Whether a value at a header key is a header this build can read, and agrees with the key it sits at. */
export function isReadableHeaderAt(key: string, value: unknown): value is BackfillHeader {
  const split = splitHeaderKey(key);
  if (!split || !isHeader(value)) return false;
  // The identity on the record and the address it was found at must agree — the same rule the popup
  // applies. A record whose two halves disagree is not one to show a user as their progress.
  if (value.platform !== split.platform || value.scope !== split.scope) return false;
  // 🔴 W91b · A stable build must not show an experimental platform's leftover state. It is not this
  //    build's progress; it belongs to the build that serves that platform.
  return isPlatformActiveInChannel(value.platform, currentReleaseChannel());
}

/** The target registry's rows, or an empty list when there is none. Rows that are not targets are dropped. */
export function targetsOf(snapshot: Record<string, unknown> | null): BackfillTarget[] {
  const raw = snapshot?.[BACKFILL_TARGETS_KEY];
  if (!Array.isArray(raw)) return [];
  return raw.filter((row): row is BackfillTarget => {
    if (!row || typeof row !== 'object') return false;
    const t = row as Partial<BackfillTarget>;
    return typeof t.platform === 'string' && typeof t.scope === 'string';
  });
}

/**
 * One (platform, scope)'s debt rows, or `null` when the store could not be read.
 *
 * 🔴 `null` and "empty" are different facts and are kept apart all the way to the page: `readDebtSet`
 *    answers `null` when IndexedDB refuses, which the model reports as "the counts below are the header's,
 *    not the authority's". Collapsing the two here would be CLAUDE.md's first invariant broken at the top
 *    of the feature that exists to show it.
 */
async function debtOf(platform: string, scope: string): Promise<CoverageDebtInput | null> {
  const snapshot = await readDebtSet(platform, scope);
  if (!snapshot) return null;
  return { pending: snapshot.pending, archived: snapshot.archived, times: snapshot.times };
}

/** Read everything and hand the model its inputs. */
export async function readCoverageInputs(
  store: BackfillStore | null,
  now: number = Date.now(),
): Promise<CoverageInput> {
  const snapshot: Record<string, unknown> | null = store
    ? await (async () => {
      try {
        return await browserLocalSnapshot();
      } catch {
        return null;
      }
    })()
    : null;

  const registered = new Set(targetsOf(snapshot).map((t) => `${t.platform}\u0000${t.scope}`));

  const headers: Array<{ platform: string; scope: string; header: BackfillHeader }> = [];
  for (const [key, value] of Object.entries(snapshot ?? {})) {
    if (!isReadableHeaderAt(key, value)) continue;
    headers.push({ platform: value.platform, scope: value.scope, header: value });
  }

  // The last wake's own record, for the "passed over, and why" half of a row's state. A tick reads the
  // registry, not individual scopes, so the reason is per-platform.
  const tick: BackfillTickRecord | null = await loadLastTick(store);
  const skippedFor = (platform: string): TickSkipReason | null => {
    const skipped = tick?.schedule?.skipped;
    if (!Array.isArray(skipped)) return null;
    return skipped.find((row) => row.platform === platform)?.reason ?? null;
  };

  const scopes: CoverageScopeInput[] = [];
  for (const { platform, scope, header } of headers) {
    scopes.push({
      platform,
      scope,
      header,
      debt: await debtOf(platform, scope),
      registered: registered.has(`${platform}\u0000${scope}`),
      skippedReason: skippedFor(platform),
    });
  }

  return {
    scopes,
    enabled: await isBackfillEnabled(store),
    hostPaused: store ? (await loadHostPause(store)) !== null : false,
    presetRaw: await readSpeedPreset(store),
    tick,
    now,
  };
}

/** The header key a row came from — for a caller that wants to name the record rather than the pair. */
export function keyOfRow(platform: string, scope: string): string {
  return stateKey(platform, scope);
}
