import { PLATFORMS } from './contract';
import type { BackfillStore } from './backfill/store';

export const CONSENT_KEY = 'cs_platform_consent_v1';
export const CONSENT_VERSION = 1 as const;

export interface PlatformConsentEntry {
  state: 'on' | 'off';
  updatedAt: number;
}

export interface PlatformConsentRecord {
  version: typeof CONSENT_VERSION;
  platforms: Record<string, PlatformConsentEntry>;
}

export interface ConsentMigration {
  key: typeof CONSENT_KEY;
  value: PlatformConsentRecord;
}

/** Every platform id the consent record governs (the compat layer's "all"). */
export function platformsForConsent(): readonly string[] {
  return PLATFORMS.map(({ id }) => id);
}

/** The one mapping from the former backfill-only switch to platform consent. */
export function migrateLegacyConsent(
  legacyBackfillEnabled: unknown,
  platformIds: readonly string[] = PLATFORMS.map(({ id }) => id),
  updatedAt = Date.now(),
): ConsentMigration {
  const state: PlatformConsentEntry['state'] = legacyBackfillEnabled === false ? 'off' : 'on';
  const platforms: Record<string, PlatformConsentEntry> = {};
  for (const id of platformIds) platforms[id] = { state, updatedAt };
  return { key: CONSENT_KEY, value: { version: CONSENT_VERSION, platforms } };
}

function isConsentRecord(value: unknown): value is PlatformConsentRecord {
  if (!value || typeof value !== 'object') return false;
  const record = value as Partial<PlatformConsentRecord>;
  if (record.version !== CONSENT_VERSION || !record.platforms || typeof record.platforms !== 'object'
    || Array.isArray(record.platforms)) return false;
  return Object.values(record.platforms).every((entry) =>
    !!entry && (entry.state === 'on' || entry.state === 'off')
    && typeof entry.updatedAt === 'number' && Number.isFinite(entry.updatedAt) && entry.updatedAt >= 0);
}

/** Missing per-platform entries mean on, as required by the default-on extension policy. */
export async function loadPlatformConsent(store: BackfillStore | null): Promise<PlatformConsentRecord> {
  if (!store) throw new Error('platform consent storage unavailable');
  const raw = await store.load(CONSENT_KEY);
  if (raw == null) {
    const migrated = migrateLegacyConsent(await store.load('cs_backfill_enabled_v1'));
    await store.save(CONSENT_KEY, migrated.value);
    return migrated.value;
  }
  if (!isConsentRecord(raw)) throw new Error('platform consent record unreadable');
  const missing = PLATFORMS.map(({ id }) => id).filter((id) => !(id in raw.platforms));
  if (missing.length === 0) return raw;
  const completed: PlatformConsentRecord = {
    ...raw,
    platforms: { ...raw.platforms },
  };
  for (const id of missing) completed.platforms[id] = { state: 'on', updatedAt: Date.now() };
  await store.save(CONSENT_KEY, completed);
  return completed;
}

export async function isPlatformConsented(
  store: BackfillStore | null,
  platformId: string,
): Promise<boolean> {
  const record = await loadPlatformConsent(store);
  return record.platforms[platformId]?.state !== 'off';
}

export async function setPlatformConsent(
  store: BackfillStore | null,
  platformId: string,
  on: boolean,
  updatedAt = Date.now(),
): Promise<PlatformConsentRecord> {
  if (!store) throw new Error('platform consent storage unavailable');
  const record = await loadPlatformConsent(store);
  const next: PlatformConsentRecord = {
    version: CONSENT_VERSION,
    platforms: { ...record.platforms, [platformId]: { state: on ? 'on' : 'off', updatedAt } },
  };
  await store.save(CONSENT_KEY, next);
  return next;
}

export async function anyPlatformConsented(store: BackfillStore | null): Promise<boolean> {
  const record = await loadPlatformConsent(store);
  return Object.values(record.platforms).some((entry) => entry.state === 'on');
}
