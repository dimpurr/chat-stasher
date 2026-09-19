/**
 * The debt set's persistence port.
 *
 * No new permissions: it reuses the browser.storage.local lib/badge.ts already
 * uses (the same cs_* key family).
 *
 * ⚠️ A known pre-existing gap (not introduced by this task, but it hits this leg
 *   directly): wxt.config.ts used to list only ['downloads'] in permissions with
 *   **no 'storage'**, while lib/badge.ts was already using browser.storage.local.
 *   The badge is decorative and silently no-oping without storage is fine;
 *   **the backfill leg is not** — without persistence there is no
 *   stop-and-resume, and a no-op all the way down becomes "crawl from scratch on
 *   every restart", exactly the silent failure we fear most.
 *   So an unavailable store returns null here, and the engine halts with
 *   'storage-unavailable' and leaves a trace rather than pretending to run.
 */

export interface BackfillStore {
  load(key: string): Promise<unknown>;
  save(key: string, value: unknown): Promise<void>;
  /**
   * 🔴 W18 · Remove a key outright. Required, not optional: the one caller is the
   * legacy-state migration's last step, and a port that quietly did not implement
   * it would leave the pre-W18 record on disk forever while every test stayed
   * green. An optional method would make "the old key was removed" unfalsifiable.
   */
  remove(key: string): Promise<void>;
  /**
   * 🔴 W36b · **Every key in the area.**
   *
   * Why this exists, and why it is required rather than optional. The W36 fix for
   * the un-migrated pre-W18 record walked the **target registry** — the scopes the
   * user is currently registered for — so a `cs_backfill_v1:<platform>:<scope>`
   * record whose scope is not in that registry was never visited by anything, and
   * neither was one on a machine whose switch is off or whose next tick is 5-10
   * minutes away. The layout is not a property of the target registry; it is a
   * property of `storage.local`, and the only thing that can enumerate it is the
   * area itself.
   *
   * 🔴 Required for the same reason `remove` is: a store that cannot list keys is
   *    a store the migration cannot run against, and a **falsey** answer would be
   *    read by the caller as "there is no pre-W18 record anywhere" — an unknown
   *    recorded as empty, which is the one thing this project does not do
   *    (CLAUDE.md invariant 1). A store that cannot answer has to say so, so the
   *    caller can say so too.
   */
  keys(): Promise<string[]>;
}

type LocalArea = {
  get: (defaults: Record<string, unknown>) => Promise<Record<string, unknown>>;
  set: (values: Record<string, unknown>) => Promise<void>;
  remove: (keys: string | string[]) => Promise<void>;
};

type ExtApi = { runtime?: { id?: string }; storage?: { local?: unknown } };

/**
 * 🔴 The **third production blocker** found by running C18 for real (not
 * introduced by this task, but run into by it):
 *
 * This used to read `globalThis.browser` only. **Chrome MV3 has no `browser`
 * global at all** — only `chrome`. So in a Chrome build browserLocalStore() was
 * always null, tickBackfill's first gate was 'no-store', and the popup's switch
 * could not be **saved** either.
 *
 * Evidence: in .output/chrome-mv3/background.js, WXT's own browser shim reads
 *   `globalThis.browser?.runtime?.id ? globalThis.browser : globalThis.chrome`
 * — lib/badge.ts uses the `browser` WXT injects (which is why the badge is fine
 * on Chrome), and only this file bypassed the shim to read globalThis, so it
 * worked on Firefox alone.
 *
 * The shim's test is copied here verbatim, with no import added (this file has to
 * run bare in the node test environment, where neither global exists ⇒ still
 * null ⇒ existing behaviour unchanged).
 */
function extensionApi(): ExtApi | null {
  const g = globalThis as { browser?: ExtApi; chrome?: ExtApi };
  if (g.browser?.runtime?.id) return g.browser;
  if (g.chrome?.runtime?.id) return g.chrome;
  // When not even runtime.id is available, fall back to whichever exists, so this is never stricter than before.
  return g.browser ?? g.chrome ?? null;
}

function localArea(): LocalArea | null {
  const area = extensionApi()?.storage?.local as LocalArea | undefined;
  if (
    !area
    || typeof area.get !== 'function'
    || typeof area.set !== 'function'
    // W18: `remove` is part of the contract too — see BackfillStore.remove. A store
    // that cannot remove a key is not one the migration may run against, and it
    // must be refused here rather than discovered half way through the migration.
    || typeof area.remove !== 'function'
  ) return null;
  return area;
}

/**
 * The one store for the one `storage.local` area.
 *
 * 🔴 W33 · Why this is memoised rather than built fresh on every call: the tab
 * registry keeps an in-memory mirror of its rows so a repeat hello can be answered
 * without a read-modify-write (lib/backfill/tab-port.ts). A mirror has to be
 * attached to *something* stable — and callers reach for `browserLocalStore()`
 * afresh on every path (background's hello handler among them), so a new object per
 * call would make every mirror cold and the optimisation dead code.
 *
 * The identity that matters is the **area**, not this function: a different
 * `storage.local` (a test swapping the fake browser, an embedder replacing the
 * object) or no area at all returns a different answer than last time, exactly as
 * before. Only the "same area, asked twice" case changed — it used to hand back two
 * objects that were indistinguishable in what they did.
 */
let cachedLocalStore: { area: LocalArea; store: BackfillStore } | null = null;

/** Returns null when storage.local is unavailable — so the caller must handle "cannot persist" explicitly. */
export function browserLocalStore(): BackfillStore | null {
  const area = localArea();
  if (!area) return null;
  if (cachedLocalStore?.area === area) return cachedLocalStore.store;
  const store: BackfillStore = {
    async load(key: string): Promise<unknown> {
      const got = await area.get({ [key]: null });
      return got[key] ?? null;
    },
    async save(key: string, value: unknown): Promise<void> {
      await area.set({ [key]: value });
    },
    async remove(key: string): Promise<void> {
      await area.remove(key);
    },
    // `get(null)` is "everything" in the real API (the same call
    // `browserLocalSnapshot` makes); a fake that does not support it throws, and
    // `migrateLegacyScopes` catches that and says it could not look rather than
    // reporting that there was nothing.
    async keys(): Promise<string[]> {
      const all = await (area.get as unknown as (q: null) => Promise<Record<string, unknown>>)(null);
      return Object.keys(all);
    },
  };
  cachedLocalStore = { area, store };
  return store;
}

/**
 * Read a full snapshot of storage.local.
 * Only the popup needs it: it does not know which platform / which account the
 * user is currently on (that information is carried by live-leg messages and
 * nowhere else), so it can only list the debt sets that already exist and pick
 * one to display.
 * An unavailable store returns null — same as browserLocalStore(), so the caller
 * handles it explicitly.
 */
export async function browserLocalSnapshot(): Promise<Record<string, unknown> | null> {
  const area = localArea();
  if (!area) return null;
  // In the real API get(null) means "everything"; a fake that does not support it will throw, which the caller catches.
  return await (area.get as unknown as (q: null) => Promise<Record<string, unknown>>)(null);
}

/** A pure in-memory implementation, for tests only. */
export function memoryStore(seed: Record<string, unknown> = {}): BackfillStore & {
  readonly data: Record<string, unknown>;
  writes: number;
} {
  const data: Record<string, unknown> = { ...seed };
  return {
    data,
    writes: 0,
    async load(key: string): Promise<unknown> {
      // Deep copy: mimics the real store's "you get back a different object" and keeps tests from sharing references by accident.
      const v = data[key];
      return v === undefined ? null : JSON.parse(JSON.stringify(v));
    },
    async save(key: string, value: unknown): Promise<void> {
      data[key] = JSON.parse(JSON.stringify(value));
      this.writes += 1;
    },
    async remove(key: string): Promise<void> {
      delete data[key];
      this.writes += 1;
    },
    async keys(): Promise<string[]> {
      return Object.keys(data);
    },
  };
}
