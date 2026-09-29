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

export function localStorageArea(): StorageArea | null {
  const g = globalThis as {
    browser?: { storage?: { local?: StorageArea } };
    chrome?: { storage?: { local?: StorageArea } };
  };
  return g.browser?.storage?.local ?? g.chrome?.storage?.local ?? null;
}
