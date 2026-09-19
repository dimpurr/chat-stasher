/**
 * W36 · **The page's link to the extension can be dead, and it must not look
 * like a quiet one.**
 *
 * ## The defect this file exists for
 *
 * A tab outlives every extension build. Reloading or updating an unpacked
 * extension does not re-inject content scripts into documents that are already
 * open, so a page that was open across a reload keeps **the previous build's
 * scripts** — including a MAIN-world hook that is still wrapping `window.fetch`.
 * The page looks perfectly armed and does the one thing it can do: it captures,
 * posts to its own isolated bridge, and the bridge tries to reach an extension
 * context that no longer exists.
 *
 * Measured, in a real Chromium, with the built extension (W36's e2e suite): after
 * `chrome.runtime.reload()` on a page left open, a fresh request was still
 * wrapped (`window.fetch` was not the browser's own), the response arrived
 * normally, and **nothing else happened at all** — no outbox entry, and not one
 * console line. The delivery failure was swallowed by an empty `catch` whose
 * comment ("a failed extension channel must never disturb the page") is right
 * about the page and wrong about the truth.
 *
 * That empty catch is the same failure CLAUDE.md's first invariant names, one
 * layer out: a conversation the user believes was archived is silently dropped,
 * and the only trace of it is an extension that reports `no-http-port` for days
 * while the platform page sits open on screen.
 *
 * ## What is done about it, and what is not
 * Chrome offers no way to re-inject without host permissions, and this manifest
 * deliberately takes none — so the capture cannot be recovered from inside the
 * page. What **can** be fixed is the silence: a stale link is a named fact, said
 * once per page (a tab left open for a week must not become a wall of identical
 * lines, the same rule lib/fallback-verification.ts states).
 *
 * 🔴 The distinction that makes this a decision and not a branch:
 *    **"the background worker is asleep"** is the normal case — Chrome wakes it
 *    for a message, the content script is right to say nothing, and the hello
 *    repeats anyway (lib/backfill/tab-hello.ts). **"the extension context is
 *    invalidated"** is not a state anything recovers from: only a page reload
 *    brings the scripts back, and until then every capture from this document is
 *    lost. Saying nothing about the second because the first is common is
 *    exactly how a silent drop stays silent.
 */

/**
 * What Chrome throws when a content script's extension context is gone (the
 * extension was reloaded, updated, or removed). Matched as a substring on
 * purpose: the surrounding sentence differs by API and by browser version
 * ("Extension context invalidated." from `runtime.sendMessage`), and a stricter
 * match would go quiet on the day a version tweaks its wording.
 */
export const INVALIDATED_CONTEXT_FRAGMENT = 'Extension context invalidated';

/**
 * The one line a stale page gets. Fixed and metadata-only: it names a state, not
 * the page, the request or anything captured (the same rule every warning in
 * this project follows).
 */
export const STALE_PAGE_LINK_WARNING =
  '[chat-stasher] this page’s link to the extension is stale'
  + ' (the extension was reloaded or updated); reload this page to capture again';

/**
 * Is this failure "the extension is gone from this document" rather than "the
 * worker was asleep"?
 *
 * Anything that is not recognisably the former is answered `false`: the caller's
 * behaviour for an unknown failure is to stay quiet, which is what every caller
 * here already did, so a shape this function does not know cannot make the
 * extension noisier than it was.
 */
export function isInvalidatedContextError(error: unknown): boolean {
  if (error === null || error === undefined) return false;
  const message = typeof error === 'string'
    ? error
    : typeof (error as { message?: unknown }).message === 'string'
      ? String((error as { message?: unknown }).message)
      : '';
  return message.includes(INVALIDATED_CONTEXT_FRAGMENT);
}

/**
 * Report a stale link **once per page**.
 *
 * The same reasoning as lib/fallback-verification.ts's gate: two occasions can
 * name the same fact (a delivery that failed, then the periodic hello that fails
 * for the same reason), and a page that keeps failing is the page that would
 * otherwise produce a wall of identical lines. The signal has to stay legible or
 * it stops being read — and a warning nobody reads is the silence this file
 * exists to remove.
 */
export function createStaleLinkWarningGate(): (error: unknown) => boolean {
  let warned = false;
  return (error: unknown): boolean => {
    if (warned) return false;
    if (!isInvalidatedContextError(error)) return false;
    warned = true;
    console.warn(STALE_PAGE_LINK_WARNING);
    return true;
  };
}
