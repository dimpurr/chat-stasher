import { BACKFILL_CURSOR_KEY, BACKFILL_TARGETS_KEY, STATE_KEY_PREFIX, LEGACY_STATE_KEY_PREFIX } from './backfill/alarm';
import { DAY_SLOW_KEY } from './backfill/day-slow';
import { BACKFILL_TABS_KEY } from './backfill/tab-port';
import { HOOK_STATUS_KEY_PREFIX } from './hook-status';
import { LIVE_CAPTURE_KEY_PREFIX } from './live-capture';
import type { BackfillStore } from './backfill/store';
import { removeOutboxEntriesForPlatform } from './outbox';
import { clearPlatformDebts } from './backfill/debt-store';

export interface PlatformLocalKeyPlan {
  remove: string[];
  edit: string[];
}

export function platformLocalKeyPlan(
  keys: readonly string[],
  platform: string,
  origins: readonly string[],
): PlatformLocalKeyPlan {
  const remove: string[] = [];
  const edit: string[] = [];
  for (const key of keys) {
    const debt = key.startsWith(`${STATE_KEY_PREFIX}${platform}:`)
      || key.startsWith(`${LEGACY_STATE_KEY_PREFIX}${platform}:`);
    const live = key === `${LIVE_CAPTURE_KEY_PREFIX}${platform}`;
    const hook = origins.some((origin) => key === `${HOOK_STATUS_KEY_PREFIX}${origin}`);
    const refusal = key === `cs_consent_refusals_v1:${platform}`;
    if (debt || live || hook || refusal) remove.push(key);
    else if (key === BACKFILL_TARGETS_KEY || key === DAY_SLOW_KEY || key === BACKFILL_TABS_KEY || key === BACKFILL_CURSOR_KEY) edit.push(key);
  }
  return { remove, edit };
}

/**
 * Clear only this platform's extension-local records. This deliberately has no
 * stage, archive, snapshot, or destination interface.
 */
export async function clearPlatformLocalData(
  store: BackfillStore,
  platform: string,
  origins: readonly string[],
): Promise<{ removedOutbox: number; removedKeys: string[]; editedKeys: string[] }> {
  const outbox = await removeOutboxEntriesForPlatform(platform);
  if (!outbox.ok) throw new Error(outbox.reason);
  const keys = await store.keys();
  const plan = platformLocalKeyPlan(keys, platform, origins);
  const debtScopes = keys.flatMap((key) => {
    const prefix = key.startsWith(`${STATE_KEY_PREFIX}${platform}:`)
      ? `${STATE_KEY_PREFIX}${platform}:`
      : key.startsWith(`${LEGACY_STATE_KEY_PREFIX}${platform}:`)
        ? `${LEGACY_STATE_KEY_PREFIX}${platform}:` : null;
    return prefix ? [key.slice(prefix.length)] : [];
  });
  if (!(await clearPlatformDebts(platform, debtScopes))) throw new Error('backfill debt storage unavailable');

  const targets = await store.load(BACKFILL_TARGETS_KEY);
  if (targets !== null && (!Array.isArray(targets) || targets.some((item) =>
    !item || typeof item !== 'object' || typeof (item as { platform?: unknown }).platform !== 'string'))) {
    throw new Error('backfill target registry unreadable');
  }
  if (Array.isArray(targets)) {
    const remaining = targets.filter((item) =>
      !item || typeof item !== 'object' || (item as { platform?: unknown }).platform !== platform);
    if (remaining.length !== targets.length) await store.save(BACKFILL_TARGETS_KEY, remaining);
  }

  const cursor = await store.load(BACKFILL_CURSOR_KEY);
  if (cursor !== null) {
    if (!cursor || typeof cursor !== 'object' || !(cursor as { served?: unknown }).served
      || typeof (cursor as { served: unknown }).served !== 'object'
      || Array.isArray((cursor as { served: unknown }).served)) throw new Error('backfill cursor unreadable');
    const served = { ...(cursor as { served: Record<string, unknown> }).served };
    const prefix = `${platform}\u0000`;
    let changed = false;
    for (const identity of Object.keys(served)) {
      if (identity.startsWith(prefix)) { delete served[identity]; changed = true; }
    }
    if (changed) await store.save(BACKFILL_CURSOR_KEY, { ...(cursor as object), served });
  }

  const slow = await store.load(DAY_SLOW_KEY);
  const slowPlatforms = slow && typeof slow === 'object' ? (slow as { platforms?: unknown }).platforms : null;
  if (slow !== null && (!slow || typeof slow !== 'object' || !slowPlatforms
    || typeof slowPlatforms !== 'object' || Array.isArray(slowPlatforms))) {
    throw new Error('day-slow record unreadable');
  }
  if (slow && typeof slow === 'object') {
    const platforms = { ...(slow as { platforms: Record<string, unknown> }).platforms };
    if (platform in platforms) {
      delete platforms[platform];
      await store.save(DAY_SLOW_KEY, { ...(slow as object), platforms });
    }
  }

  const tabRows = await store.load(BACKFILL_TABS_KEY);
  if (tabRows !== null && (!Array.isArray(tabRows) || tabRows.some((item) =>
    !item || typeof item !== 'object' || typeof (item as { origin?: unknown }).origin !== 'string'))) {
    throw new Error('backfill tab registry unreadable');
  }
  if (Array.isArray(tabRows)) {
    const remaining = tabRows.filter((item) =>
      !item || typeof item !== 'object' || !origins.includes(String((item as { origin?: unknown }).origin ?? '')));
    if (remaining.length !== tabRows.length) await store.save(BACKFILL_TABS_KEY, remaining);
  }

  for (const key of plan.remove) await store.remove(key);
  return { removedOutbox: outbox.removed, removedKeys: plan.remove, editedKeys: plan.edit };
}
