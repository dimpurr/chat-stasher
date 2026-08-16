/**
 * Q2 visibility: one source of truth for the toolbar badge.
 *
 * DESIGN DECISION — stale badge after SW death:
 *  - A service worker's badge is native UI: once set, it stays even after the
 *    SW is killed. If we only ever incremented it, a dead worker would leave an
 *    out-of-date number behind and the user couldn't tell capture had stopped.
 *  - Fix chosen: persist {lastCaptureAt, count} to storage.local on every
 *    successful capture. Whenever the SW materialises (startup / onStartup /
 *    next capture) we compare lastCaptureAt against now (>5 min = stale) and,
 *    if stale, CLEAR the badge (empty text) and reset the counters. Empty text
 *    was chosen over a grey badge: badge colour renders differently across
 *    browser themes and a greyed digit could still read as "quietly running";
 *    an empty badge is the only unambiguous "off" signal.
 *  - While the SW is dead the badge natively keeps its last value — nothing can
 *    repaint it without waking the SW. The rule re-asserts correctness the
 *    moment any capture event or startup fires again.
 */

export const BADGE_STALE_MS = 5 * 60 * 1000;
export const BADGE_KEY_COUNT = 'cs_count';
export const BADGE_KEY_LAST_AT = 'cs_last_capture_at';
export const BADGE_COLOR_ACTIVE = '#c1353c';

export function isStale(
  lastCaptureAt: number | undefined,
  now: number,
  staleMs: number = BADGE_STALE_MS,
): boolean {
  if (lastCaptureAt == null) return true;
  return now - lastCaptureAt > staleMs;
}

export interface BadgeState {
  count: number;
  lastCaptureAt: number | undefined;
}

/** Signature-agnostic small helpers so tests can stub only what they use. */
function storageApi() {
  // Badge is cosmetic: if the storage/action surface is absent on this browser
  // build (or in a test mock), every helper becomes a silent no-op.
  return ((browser as { storage?: { local?: unknown } }).storage?.local ?? null) as {
    get: (defaults: Record<string, unknown>) => Promise<Record<string, unknown>>;
    set: (v: Record<string, unknown>) => Promise<void>;
    remove: (k: string[]) => Promise<void>;
  } | null;
}

async function getLocal(defaults: Record<string, unknown>): Promise<Record<string, unknown>> {
  const api = storageApi();
  if (!api || typeof api.get !== 'function') return defaults;
  return await api.get(defaults);
}

async function setLocal(values: Record<string, unknown>): Promise<void> {
  const api = storageApi();
  if (!api || typeof api.set !== 'function') return;
  await api.set(values);
}

async function removeLocal(keys: readonly string[]): Promise<void> {
  const api = storageApi();
  if (!api || typeof api.remove !== 'function') return;
  await api.remove(keys as string[]);
}

export async function loadBadgeState(): Promise<BadgeState> {
  const got = await getLocal(
    { [BADGE_KEY_COUNT]: 0, [BADGE_KEY_LAST_AT]: 0 },
  );
  const count = typeof got[BADGE_KEY_COUNT] === 'number' ? (got[BADGE_KEY_COUNT] as number) : 0;
  const lastCaptureAt = typeof got[BADGE_KEY_LAST_AT] === 'number'
    ? (got[BADGE_KEY_LAST_AT] as number)
    : undefined;
  return { count, lastCaptureAt };
}

export async function setActiveBadge(count: number): Promise<void> {
  const action = (browser as { action?: { setBadgeText: (o: { text: string }) => Promise<void>; setBadgeBackgroundColor: (o: { color: string }) => Promise<void> } }).action;
  if (!action) return; // e.g. Firefox MV2 alias — badge is cosmetic, never fatal
  await action.setBadgeText({ text: String(count) });
  await action.setBadgeBackgroundColor({ color: BADGE_COLOR_ACTIVE });
}

export async function clearBadge(): Promise<void> {
  const action = (browser as { action?: { setBadgeText: (o: { text: string }) => Promise<void> } }).action;
  if (!action) return;
  await action.setBadgeText({ text: '' });
}

/**
 * Called once per successful capture: bump the count, stamp the time, paint the
 * badge. Storage keys live in the same session namespace so a mid-session SW
 * recycle restores the visible count instead of starting over.
 */
export async function recordCapture(): Promise<void> {
  const prev = await loadBadgeState();
  const count = prev.count + 1;
  const now = Date.now();
  await setLocal({ [BADGE_KEY_COUNT]: count, [BADGE_KEY_LAST_AT]: now });
  await setActiveBadge(count);
}

/**
 * Re-assert the badge's truth on every SW wake. Stale (or never-tracked) =>
 * clear badge + reset session counters; fresh => repaint the persisted count.
 */
export async function refreshBadge(): Promise<void> {
  const state = await loadBadgeState();
  if (isStale(state.lastCaptureAt, Date.now())) {
    await removeLocal([BADGE_KEY_COUNT, BADGE_KEY_LAST_AT]);
    await clearBadge();
  } else if (state.count > 0) {
    await setActiveBadge(state.count);
  } else {
    await clearBadge();
  }
}