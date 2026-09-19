/**
 * W43 · **The record of a page where the capture hook did not do its job.**
 *
 * Why this file exists, in the product's own terms. Until W43 there was exactly
 * one way to learn that the hook had not been installed on a page: nothing
 * happened. A page where the hook never ran produced no capture, which is the
 * same observable state as a page where the user simply opened no conversation —
 * and on 2026-09-19 a real logged-in Chrome showed exactly that on two platforms,
 * with no way to tell it apart from the quiet case. The tool's whole reason for
 * existing is that "I did not find it" and "there was nothing to find" are
 * different facts (CLAUDE.md invariant 1), and this was the one place the
 * extension itself collapsed them.
 *
 * So: an observation the extension makes **itself, in the page that made it**, is
 * written down here, per origin, and read back by the popup.
 *
 * ## What is recorded, and what is deliberately not
 *
 *  · Only observations, never diagnoses. Each reason below says what was seen;
 *    none of them names a cause. "The page never answered" is a fact. "The site
 *    enabled Trusted Types" is a hypothesis — and the 2026-09-19 measurement that
 *    prompted this file left the cause of one of the two sites **unsettled** (see
 *    the W43 report), so nothing here may close that gap by wording.
 *  · The origin, the platform id, the reason, and the time. **No URL path, no
 *    conversation id, no body, no token.** The origin is the one identifier a
 *    reader needs to open the tab that has the problem, and it is already in the
 *    platform table this extension ships with.
 *  · This module never decides "the hook is not installed" from an absence of
 *    captures. Absence of traffic is not evidence of a broken hook, and reading it
 *    as one would file a failure for every quiet page. The only writers are the
 *    two parties that can see the hook's state directly: the hook itself, and the
 *    bridge whose probe it answers.
 *
 * ## Where the vocabulary lives
 *
 * The three reason codes are declared in `lib/contract.ts`, not here, because
 * the page-world hook has to name them too and contract.ts is the only module it
 * shares with the extension side (and the only one it may share: the page world
 * gets no extension APIs, and this file's wording is resolved through
 * `lib/i18n.ts`). This file adds the part that only the extension side needs —
 * the record's shape, its storage key, its merge, its readers, and the one writer
 * that touches storage — through the `BackfillStore` port `lib/host-status.ts`
 * also takes, so a test supplies a fake instead of a browser. No i18n: the popup
 * resolves the wording, and this module never sees a sentence.
 */

import {
  HOOK_OBSERVATIONS,
  isHookObservation,
  type HookObservation,
} from './contract';
import type { BackfillStore } from './backfill/store';

/** Storage key prefix. One record per origin; at most one per platform row. */
export const HOOK_STATUS_KEY_PREFIX = 'cs_hook_v1:';

export { HOOK_OBSERVATIONS, isHookObservation };
export type { HookObservation };

/** One observation and when it was made (`Date.now()` ms). */
export interface HookObservationRecord {
  reason: HookObservation;
  at: number;
}

/**
 * One origin's record. `reasons` holds at most one entry per member of the
 * closed set — a repeated observation updates its timestamp rather than
 * appending, so the size is bounded by the vocabulary and not by uptime. `at` is
 * the most recent observation of any reason, so "when did this page last fail"
 * is one field to read.
 */
export interface HookStatusRecord {
  origin: string;
  platform: string;
  reasons: HookObservationRecord[];
  at: number;
}

export function hookStatusKey(origin: string): string {
  return `${HOOK_STATUS_KEY_PREFIX}${origin}`;
}

/** Strict enough that junk in storage is never read as a record. */
export function looksLikeHookStatus(value: unknown): value is HookStatusRecord {
  if (!value || typeof value !== 'object') return false;
  const record = value as Record<string, unknown>;
  if (typeof record.origin !== 'string' || record.origin.length === 0) return false;
  if (typeof record.platform !== 'string' || record.platform.length === 0) return false;
  if (typeof record.at !== 'number' || !Number.isFinite(record.at)) return false;
  if (!Array.isArray(record.reasons) || record.reasons.length === 0) return false;
  return record.reasons.every((entry) => {
    if (!entry || typeof entry !== 'object') return false;
    const row = entry as Record<string, unknown>;
    return isHookObservation(row.reason) && typeof row.at === 'number' && Number.isFinite(row.at);
  });
}

/**
 * Merge one observation into a record (or start one). **Pure**: the caller
 * persists what comes back. A reason already recorded has its timestamp replaced
 * — the fact is the same fact, and the newest time is the one that answers "is
 * this still happening".
 */
export function mergeHookObservation(
  existing: HookStatusRecord | null,
  observation: { origin: string; platform: string; reason: HookObservation; at: number },
): HookStatusRecord {
  // A record stored under a different origin is not this origin's record; start
  // a fresh one rather than merging two pages into one row.
  const base = existing && existing.origin === observation.origin ? existing : null;
  const reasons = (base?.reasons ?? []).filter((row) => row.reason !== observation.reason);
  reasons.push({ reason: observation.reason, at: observation.at });
  // Sorted by the closed set, so the order is a property of the vocabulary and
  // not of the order things happened to be noticed in.
  reasons.sort((a, b) => HOOK_OBSERVATIONS.indexOf(a.reason) - HOOK_OBSERVATIONS.indexOf(b.reason));
  return {
    origin: observation.origin,
    platform: observation.platform,
    reasons,
    at: observation.at,
  };
}

/**
 * Every hook record in a storage snapshot, newest first.
 *
 * 🔴 A value under the prefix that is not a record is **not** repaired, guessed
 *    at, or turned into a record with no reasons: it is left alone and left out
 *    of this list, and the popup says nothing about it (the same rule as
 *    `backfillStateEntries` in lib/popup-view.ts). A key whose origin disagrees
 *    with the record inside it is dropped for the same reason — the two are one
 *    fact, and a row that contradicts itself is not a row to show a user.
 */
export function hookStatusOf(snapshot: Record<string, unknown> | null): HookStatusRecord[] {
  if (!snapshot) return [];
  const out: HookStatusRecord[] = [];
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(HOOK_STATUS_KEY_PREFIX)) continue;
    if (!looksLikeHookStatus(value)) continue;
    if (key !== hookStatusKey(value.origin)) continue;
    out.push(value);
  }
  out.sort((a, b) => b.at - a.at);
  return out;
}

/**
 * Write one page's observation down — or, for `reason: null`, clear the origin's
 * record because that page's hook **verified**.
 *
 * 🔴 The clear is evidence, not acknowledgement, and that is why there is no
 *    button for it. A record the user could dismiss without the condition having
 *    changed would be a note that says "this was broken" about a page that is
 *    still broken — which is the failure mode invariant 1 exists to prevent, one
 *    level up. What clears it is the only thing that can honestly clear it: a
 *    page on that origin whose hook answered a probe. Reloading the tab is how a
 *    user makes that happen, and the popup's sentence says so.
 *
 * 🔴 `remove`, not "save an empty record". The two look the same on the next read
 *    and are not the same thing on disk: an empty record is a row that has to be
 *    filtered out by every reader forever, and its very presence would mean "a
 *    page on this origin once said something" — which is not what we know any
 *    more.
 *
 * A store that cannot be written or read is **not** a reason to throw into a
 * content-script reply path: it is logged, and the record stays as it was.
 */
export async function recordHookStatus(
  store: BackfillStore | null,
  observation: { origin: string; platform: string; reason: HookObservation | null; at: number },
): Promise<void> {
  if (!store) return;
  const key = hookStatusKey(observation.origin);
  try {
    if (observation.reason === null) {
      await store.remove(key);
      return;
    }
    const stored = await store.load(key);
    await store.save(
      key,
      mergeHookObservation(looksLikeHookStatus(stored) ? stored : null, {
        origin: observation.origin,
        platform: observation.platform,
        reason: observation.reason,
        at: observation.at,
      }),
    );
  } catch (err) {
    console.warn('[chat-stasher] hook status write failed', (err as Error).message);
  }
}
