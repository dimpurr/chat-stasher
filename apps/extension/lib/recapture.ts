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
 *
 * 🔴 W50c · **The answer comes from the archive, and this module only asks for it.**
 *
 * Two review rounds found that no arrangement of this store could be trusted to
 * answer the question it was being asked. W50 keyed a record by delivery name and
 * content; W50b added the stage and machine the host reported at ack time. Both were
 * still *extension-side memory about an archive*, and a memory cannot see the event
 * that matters: an archive that was replaced or restored at the same path leaves
 * every record standing, and a relisted conversation is then settled as archived
 * without one byte reaching the archive that is there now.
 *
 * The rule is now: **the question "is this exact content already stored?" is answered
 * by the native host, from its stage** (protocol §6.6, `lib/native-host.ts` `has`).
 * What is left here is
 *  · the fingerprint itself — the one thing the host cannot derive (see
 *    [`contentFingerprint`]);
 *  · the record of "this content was delivered at least once", used **only** to
 *    decide whether asking is worth a round trip. It cannot cause a skip: a skip
 *    needs a `held: true` from the host for this capture's own `(platform,
 *    sessionId, fingerprint)`.
 *
 * 🔴 The record no longer carries a destination, and that is W50b's field being
 *    retired rather than forgotten. "Where" was the extension's attempt to answer a
 *    question only the archive can answer, and a *stale* "where" is worse than none:
 *    the host is now asked about the stage it is writing to, so a stage that moved
 *    answers for itself. See [`isUnchangedCapture`] for the gate this record is.
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

/**
 * 🔴 W50c · **What the host is asked, and the only shape of answer this module
 * accepts.** One function signature, so both delivery legs ask the same question
 * the same way and neither can invent a cheaper one.
 *
 * `sessionId` is the bundle's own, not a value re-derived here: the host scopes its
 * answer to the directory a `deliver` of that bundle would write into, and it builds
 * that name from the bundle's `platform`/`sessionId` through its own single
 * derivation. Sending anything else could only look in the wrong directory, which
 * answers `held: false` — an extra copy, never a wrong skip.
 */
export type HostHoldLookup = (query: {
  platform: string;
  sessionId: string;
  fingerprint: string;
}) => Promise<boolean>;

/** A fingerprint is 64 lowercase hex characters, here and on the wire. */
const FINGERPRINT_RE = /^[0-9a-f]{64}$/;

type Remembered = Record<string, string>;

/**
 * The fingerprint `value` records, or `null` when `value` is not a record of one.
 *
 * 🔴 Both of these shapes are read, and reading the older one is not a migration
 *    that fills in a missing field — it discards one.
 *
 *  · a bare fingerprint string: what this store held before W50b;
 *  · `{fingerprint, machine, stage}`: W50b's shape. Its `machine`/`stage` are read
 *    past, because under W50c the extension makes no claim about *where* a delivery
 *    went: the host answers that about its own stage, and a remembered "where" can
 *    only be stale (a replaced archive at the same path is exactly the case that
 *    fooled it). A fingerprint from either shape says the same one thing — "this
 *    content was delivered at least once from this profile" — and that is all this
 *    record is allowed to mean.
 *
 * 🔴 W50b read the bare string as "not a record" and delivered again, on the grounds
 *    that a record which cannot say where it went cannot answer "unchanged". That
 *    reasoning is what W50c retires: this store never answers "unchanged" any more,
 *    so a value it cannot fully interpret is not a value that might be wrong — it is
 *    a value that says "asking is worth it", which the host then answers for real.
 */
function parseDeliveredFingerprint(value: unknown): string | null {
  if (typeof value === 'string') return FINGERPRINT_RE.test(value) ? value : null;
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const fingerprint = (value as Record<string, unknown>).fingerprint;
  return typeof fingerprint === 'string' && FINGERPRINT_RE.test(fingerprint) ? fingerprint : null;
}

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
 * 🔴 W50c · **The fingerprint of the payload a delivery is about to send**, or
 * `null` when the payload is not the shape one can be derived from.
 *
 * Why this is derived from the payload and not carried alongside it: the fingerprint
 * is what the host records *on the sealed shard* and later answers `has` from, so
 * deriving it from the very bytes being sealed is what makes "this shard's
 * fingerprint" and "this shard's `raw.text`" unable to disagree. It is also the one
 * delivery point that covers **both** legs: the live leg reaches the host through the
 * outbox drain (`lib/outbox.ts`) and the backfill leg calls `deliver` directly, so a
 * value carried by the callers would have to be threaded through a queued record —
 * and would then be absent for every entry queued before this version.
 *
 * Two calls therefore compute the same value — once here, at delivery, and once from
 * the capture for the pre-gate above. That is not the "one fact, two expressions"
 * failure this repository has removed twice (C21, W50): the inputs are the same
 * function's, over the same strings, and `raw.text` is the captured body verbatim.
 * 🔴 And if they ever did differ, the direction is safe — a shard whose fingerprint
 *    the pre-gate does not recognise answers `held: false`, which delivers once more.
 */
export async function deliveryFingerprint(payload: string): Promise<string | null> {
  let bundle: unknown;
  try {
    bundle = JSON.parse(payload);
  } catch {
    return null;
  }
  if (typeof bundle !== 'object' || bundle === null || Array.isArray(bundle)) return null;
  const { platform, raw } = bundle as { platform?: unknown; raw?: unknown };
  if (typeof platform !== 'string') return null;
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return null;
  const text = (raw as { text?: unknown }).text;
  if (typeof text !== 'string') return null;
  return contentFingerprint(platform, text);
}

/**
 * 🔴 W50c · **The one rule for "skip this capture", and it ends in the host's
 * answer.**
 *
 * Two conditions, and the second is the decision:
 *  1. this profile remembers delivering this exact fingerprint under this name — a
 *     **cheap pre-gate**, and nothing more. Its absence costs one delivery of
 *     something the archive may already hold: the safe direction, and the price of
 *     not asking a native process about every capture the browser ever makes;
 *  2. `askHost` answers `true` — **the host holds this content in the stage it
 *     writes to right now.**
 *
 * Nothing else skips, and no caller may substitute an answer of its own for step 2:
 * a null fingerprint is answered `false` here rather than being left to each caller,
 * because the two legs skipping on different terms is exactly the drift W50 removed.
 *
 * 🔴 Why step 1 cannot be the answer (the two review findings, structurally): it is
 *    extension storage, and extension storage does not change when the archive does.
 *    Replacing or restoring an archive at the same path leaves the record standing,
 *    and a record W50b *taught* to name the destination cannot help — the destination
 *    it names is a string the extension wrote down, and the host now writes
 *    somewhere else. Asking the host is asking the archive.
 *
 * 🔴 **Every** other outcome of step 2 is `false` ⇒ deliver: `held: false`, a `nack`
 *    (including an old host's "unknown message type"), a timeout, a send failure, a
 *    malformed response, a response whose `request_id` is not the one we sent. There
 *    is no "probably stored" branch. An unreadable store is answered `false` for the
 *    same reason: not known to be unchanged ⇒ deliver, one extra copy, never a
 *    change that went unnoticed.
 *
 * The pre-gate is paid for by the platform id it does not need and the round trip it
 * does not make: a first-time capture of a conversation is delivered without asking
 * anything, and a relist — the case this exists for — trades one native-message round
 * trip for a multi-megabyte duplicate.
 */
export async function isUnchangedCapture(
  store: BackfillStore | null,
  name: string,
  derived: CaptureFingerprint,
  identity: { platform: string; sessionId: string },
  askHost: HostHoldLookup,
): Promise<boolean> {
  if (derived.fingerprint === null) return false;

  let remembered: string | null;
  try {
    remembered = await rememberedFingerprint(store, name);
  } catch {
    // Unreadable ⇒ not known to be unchanged ⇒ deliver.
    return false;
  }
  // The cheap gate: with no record, or a different body, nothing the host could say
  // would make this capture unchanged — so it is never asked about them.
  if (remembered !== derived.fingerprint) return false;

  try {
    return await askHost({
      platform: identity.platform,
      sessionId: identity.sessionId,
      fingerprint: derived.fingerprint,
    });
  } catch {
    // A lookup that throws is a lookup that did not answer.
    return false;
  }
}

/**
 * The fingerprint this name was last acknowledged under, or `null` when this store
 * holds no record for it.
 *
 * 🔴 A **fingerprint** comes back rather than a boolean, so the comparison with what
 *    is being captured now stays in one place (the caller, above) instead of being
 *    split between a reader and a caller that would each have to remember it.
 */
async function rememberedFingerprint(
  store: BackfillStore | null,
  name: string,
): Promise<string | null> {
  return (await load(store))[name] ?? null;
}

/**
 * Every well-formed record in the store, keyed by delivery name. Values that record
 * no fingerprint are dropped — see `parseDeliveredFingerprint`, which is where the
 * shapes this store has had are read for the one thing they all mean.
 */
async function load(store: BackfillStore | null): Promise<Remembered> {
  if (!store) return {};
  const raw = await store.load(LAST_DELIVERED_KEY);
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  const out: Remembered = {};
  for (const [key, value] of Object.entries(raw as Record<string, unknown>)) {
    const fingerprint = parseDeliveredFingerprint(value);
    if (fingerprint) out[key] = fingerprint;
  }
  return out;
}

/**
 * Record an acknowledged fingerprint for `name` (most recent last; oldest dropped
 * past the cap).
 */
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
 *  · a null fingerprint records nothing (there is nothing to ask the host about next
 *    time);
 *  · a store that cannot be written **never throws into the delivery path**. A
 *    missing record costs one extra copy of a conversation; the delivery's own
 *    outcome is not allowed to depend on it. That was the live leg's rule and it is
 *    now the backfill leg's too, because the backfill leg's answer settles a debt
 *    (engine.ts `sinkVerdict` → `settleDebt` / `recordFailure`) and an exception
 *    here would turn a stored conversation into a failure.
 *
 * 🔴 It is called only where a delivery has been **acknowledged**, and never before:
 *    a copy that was merely queued or merely attempted proves nothing, and a
 *    pre-gate raised on that basis would ask the host about a conversation that was
 *    never stored — the host would answer `held: false` and nothing would be lost,
 *    but the record would be a claim about our own behaviour that is not true.
 *    The call sites differ because the two legs acknowledge differently — the live
 *    leg after `lookup.entry === null` (a matching ack deleted its outbox entry),
 *    the backfill leg after `result.delivered` (it does not go through the outbox at
 *    all, so it has no such entry to observe) — but the write itself is this one
 *    function on both.
 *
 * 🔴 W50c · **The destination is gone from the call, the signature and the record.**
 *    W50b passed it in so a record could answer "at *this* destination"; that answer
 *    is now the host's, about its own stage, and a second copy of it here could only
 *    ever disagree with the archive. Nothing this function writes claims to know
 *    where anything went.
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
