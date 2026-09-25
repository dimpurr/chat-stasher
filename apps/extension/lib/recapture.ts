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
import { findPlatformForUrl } from './contract';
import type { BackfillStore } from './backfill/store';

export const LAST_DELIVERED_KEY = 'cs_last_delivered_v1';
/** How many conversations to remember; the oldest are forgotten first. */
export const MAX_REMEMBERED = 2000;

/**
 * Per platform: top-level response fields that change on every request.
 *
 * 🔴 W21 · **No entry for grok, and that is a decision with a reason.** The
 *    content response (`{ responses: [...] }`) carries responseId, message,
 *    sender, createTime, parentResponseId, model and the web-search/file
 *    metadata, and **no source shows a field of the kind this table exists for**
 *    — no signed URL, no URL that expires, no "fetched at" stamp on the envelope
 *    (the same sources do show a signed URL in another platform's file metadata,
 *    which is why the absence here was looked for rather than assumed).
 *    Registering a key with no evidence would be worse than registering none: a
 *    wrong key is not neutral, it **deletes that field before comparing**, so a
 *    real change inside it would be skipped and the archive would keep the old
 *    copy while the user was told nothing had changed.
 *
 *    What is therefore left in place for grok, stated rather than hidden: the
 *    sources note that `responses` is **not guaranteed to be in request order**,
 *    so two views of one unchanged conversation can serialise differently and
 *    re-deliver. This table cannot express "order an array" (it deletes fields,
 *    never normalises), and the safe direction of that error is an extra copy —
 *    never a change that went unnoticed.
 *
 * 🔴 W22 · **No entry for kimi either, and the reason this one was looked for
 *    rather than assumed.** The measured detail response is `{ messages: [...] }`
 *    and each message carries `id, parentId, role, status, blocks, scenario,
 *    createTime, isGoal` (observed in a logged-in session on 2026-09-14). The
 *    question this table exists to answer is "does anything in there change on
 *    every fetch?", and the honest answer is: **nothing observed does** — no
 *    signed URL, no URL with an expiry, no fetch-time stamp appeared anywhere in
 *    what was measured, on the envelope or on a message.
 *
 *    Two things are deliberately NOT done about the gap that leaves:
 *     · a key is not registered on the strength of "blocks usually carry images
 *       and images usually carry signed URLs" — that is a guess about a field
 *       whose contents were not enumerated, and a wrong key here is not neutral:
 *       `contentFingerprint` **deletes** it before comparing, so a real change
 *       inside it would be skipped while the user was told nothing had changed;
 *     · the residual is not hidden either: if a message's `blocks` do carry
 *       per-fetch URLs, two views of one unchanged conversation serialise
 *       differently and the second one is delivered as a new copy. The safe
 *       direction of that error is an extra copy, and the raw body stays
 *       authoritative — but this table could not fix it even if the URLs were
 *       known, because it deletes **top-level** keys and cannot reach inside an
 *       array of blocks. Recording that limit here is the point of this note.
 */
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

/**
 * 🔴 W50 · **"Which platform is this capture, and what is the fingerprint of its
 * body" — derived by one function, used by both delivery legs.**
 *
 * Why it is one function and not one expression per leg: the fingerprint is only
 * meaningful per platform (each has its own volatile-field table above), and the
 * delivery name is built from the same platform id (`preparePayload`). The live
 * leg's own comment already named the failure this prevents — *"'which platform is
 * this capture' is one question with one answer, and a second derivation of it here
 * is how two expressions of one fact drift apart"* (the failure C21 removed from the
 * identity path). The backfill leg now asks the same function instead of repeating
 * the expression, so the two legs cannot come to different answers.
 */
export interface CaptureFingerprint {
  /** null ⇒ not a platform we fingerprint ⇒ this capture is never skipped. */
  platform: string | null;
  /**
   * null ⇒ there is nothing to compare against, and the capture is never skipped:
   * a platform with no registered volatile fields (grok, kimi, deepseek — see the
   * notes above), a body that is not a JSON object, or a URL that names no platform.
   * 🔴 "We have no fingerprint" is not "unchanged" — it is unknown, and an unknown
   *    must never be recorded as unchanged (invariant 1). The null therefore flows
   *    all the way to "deliver".
   */
  fingerprint: string | null;
}

export async function captureFingerprint(
  captured: { url: string; text: string },
): Promise<CaptureFingerprint> {
  const platform = findPlatformForUrl(captured.url)?.id ?? null;
  if (!platform) return { platform: null, fingerprint: null };
  return { platform, fingerprint: await contentFingerprint(platform, captured.text) };
}

/**
 * 🔴 W50 · **The one rule for "skip this capture": there is a fingerprint, and it
 * matches the one written down for this delivery name.**
 *
 * Nothing else skips: a null fingerprint is answered `false` here rather than being
 * left to each caller, because the two legs skipping on different terms is exactly
 * the drift this task exists to remove.
 *
 * An unreadable store is answered `false` too (`isUnchangedSinceDelivery` catches
 * it): not known to be unchanged ⇒ deliver. The safe direction of that error is one
 * extra copy, never a change that went unnoticed.
 */
export async function isUnchangedCapture(
  store: BackfillStore | null,
  name: string,
  derived: CaptureFingerprint,
): Promise<boolean> {
  if (derived.fingerprint === null) return false;
  return await isUnchangedSinceDelivery(store, name, derived.fingerprint);
}

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

/**
 * 🔴 W50 · **The one place a delivered fingerprint is written down, on either leg.**
 *
 * Two things are one place here rather than two:
 *  · a null fingerprint records nothing (there is nothing to compare next time);
 *  · a store that cannot be written **never throws into the delivery path**. A
 *    missing record costs one extra copy of a conversation; the delivery's own
 *    outcome is not allowed to depend on it. That was the live leg's rule and it is
 *    now the backfill leg's too, because the backfill leg's answer settles a debt
 *    (engine.ts `sinkVerdict` → `settleDebt` / `recordFailure`) and an exception
 *    here would turn a stored conversation into a failure.
 *
 * 🔴 It is called only where a delivery has been **acknowledged**, and never before:
 *    a copy that was merely queued or merely attempted proves nothing, and skipping
 *    it next time on that basis would lose a conversation that was never stored.
 *    The call sites differ because the two legs acknowledge differently — the live
 *    leg after `lookup.entry === null` (a matching ack deleted its outbox entry),
 *    the backfill leg after `result.delivered` (it does not go through the outbox at
 *    all, so it has no such entry to observe) — but the write itself is this one
 *    function on both.
 *
 * The log line carries metadata only (platform + the fingerprint's first 12 hex
 * characters): never a URL, an id or a body.
 */
export async function rememberDeliveredQuietly(
  store: BackfillStore | null,
  name: string,
  derived: CaptureFingerprint,
): Promise<void> {
  if (derived.fingerprint === null) return;
  try {
    await rememberDelivered(store, name, derived.fingerprint);
  } catch (err) {
    console.warn(
      '[chat-stasher] could not record the delivered fingerprint for'
      + ` ${derived.platform}/${derived.fingerprint.slice(0, 12)}`,
      (err as Error).message,
    );
  }
}
