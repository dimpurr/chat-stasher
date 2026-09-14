/**
 * Skip re-delivering a conversation that has not changed since it was last
 * acknowledged.
 *
 * Measured 2026-09-14: ChatGPT re-sends the full conversation on every view,
 * and two copies of the same conversation differed only in the top-level
 * `safe_urls` field (current_node, update_time and all 451 mapping nodes were
 * identical). Each view therefore appended another full copy to the archive —
 * 4 × 2.1 MB for one conversation in 20 minutes.
 *
 * The fingerprint is the sha256 of the response with only the fields listed in
 * VOLATILE_KEYS removed; any other difference, however small, is a change and
 * is delivered. A fingerprint is recorded only after the host acknowledged the
 * delivery, so an unconfirmed copy is never skipped.
 */
import { sha256Hex } from './native-host';
import type { BackfillStore } from './backfill/store';

export const LAST_DELIVERED_KEY = 'cs_last_delivered_v1';
/** How many conversations to remember; the oldest are forgotten first. */
export const MAX_REMEMBERED = 2000;

/** Per platform: top-level response fields that change on every request. */
const VOLATILE_KEYS: Readonly<Record<string, readonly string[]>> = {
  chatgpt: ['safe_urls'],
};

/**
 * null when this platform has no known volatile fields, or the body is not a
 * JSON object — in both cases nothing is skipped.
 */
export async function contentFingerprint(platform: string, text: string): Promise<string | null> {
  const volatile = VOLATILE_KEYS[platform];
  if (!volatile) return null;
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return null;
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) return null;
  const stable: Record<string, unknown> = { ...(body as Record<string, unknown>) };
  for (const key of volatile) delete stable[key];
  return sha256Hex(`${platform}\n${JSON.stringify(stable)}`);
}

type Remembered = Record<string, string>;

async function load(store: BackfillStore | null): Promise<Remembered> {
  if (!store) return {};
  const raw = await store.load(LAST_DELIVERED_KEY);
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  const out: Remembered = {};
  for (const [key, value] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof value === 'string') out[key] = value;
  }
  return out;
}

/** True only when this exact fingerprint was acknowledged for this delivery name. */
export async function isUnchangedSinceDelivery(
  store: BackfillStore | null,
  name: string,
  fingerprint: string,
): Promise<boolean> {
  try {
    return (await load(store))[name] === fingerprint;
  } catch {
    // Unreadable ⇒ not known to be unchanged ⇒ deliver.
    return false;
  }
}

/** Record an acknowledged fingerprint (most recent last; oldest dropped past the cap). */
export async function rememberDelivered(
  store: BackfillStore | null,
  name: string,
  fingerprint: string,
): Promise<void> {
  if (!store) return;
  const current = await load(store);
  delete current[name];
  current[name] = fingerprint;
  const keys = Object.keys(current);
  for (const key of keys.slice(0, Math.max(0, keys.length - MAX_REMEMBERED))) delete current[key];
  await store.save(LAST_DELIVERED_KEY, current);
}
