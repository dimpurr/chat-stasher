/**
 * `storage.local`, resolved for both browser spellings.
 *
 * Firefox exposes `browser`, Chrome exposes `chrome`, and a test can stub
 * either; `browser.storage.local ?? chrome.storage.local` is the lookup every
 * module here needs, so it lives in one place. A null area is not an empty
 * area: every caller has to say what it does when the store cannot be read,
 * and this module deliberately cannot answer that for them.
 */
export interface StorageArea {
  get(keys: Record<string, unknown>): Promise<Record<string, unknown>>;
  set(items: Record<string, unknown>): Promise<void>;
}

function storageArea(value: unknown): StorageArea | null {
  if (typeof value !== 'object' || value === null) return null;

  const area = value as { get?: unknown; set?: unknown };
  return typeof area.get === 'function' && typeof area.set === 'function'
    ? (value as StorageArea)
    : null;
}

function localStorageCandidate(value: unknown): StorageArea | null {
  if (typeof value !== 'object' || value === null) return null;

  try {
    const storage = (value as { storage?: unknown }).storage;
    if (typeof storage !== 'object' || storage === null) return null;

    return storageArea((storage as { local?: unknown }).local);
  } catch {
    return null;
  }
}

export function localStorageArea(): StorageArea | null {
  const g = globalThis as unknown as { browser?: unknown; chrome?: unknown };
  return localStorageCandidate(g.browser) ?? localStorageCandidate(g.chrome);
}
