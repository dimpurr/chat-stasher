/**
 * Runs before every test file.
 *
 * Why this exists at all: `@wxt-dev/browser` decides once, when it is first
 * imported, whether the extension APIs come from `globalThis.browser` or
 * `globalThis.chrome` (node_modules/@wxt-dev/browser/src/index.mjs), and
 * `@wxt-dev/i18n` then reads `browser.i18n.getMessage` through that captured
 * object. A suite that replaces `globalThis.browser` with its own fake without
 * carrying an `i18n` member would therefore make the English path throw — not
 * because the extension is wrong, but because the fake is incomplete. So the
 * members the translation layer reaches for are installed here, once, for every
 * suite; suites that build their own fake browser complete it with
 * `withI18n()` from tests/i18n-harness.ts.
 *
 * Assignment rather than `vi.stubGlobal` on purpose: suites call
 * `vi.unstubAllGlobals()` between cases, and the translation layer has to
 * survive that — it is not what those suites are testing.
 */

import { beforeEach } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';

import { catalogFetch, i18nApi, runtimeApi, TEST_EXTENSION_ORIGIN } from './i18n-harness';

const g = globalThis as Record<string, unknown>;

// 🔴 Installed on `chrome`, never on `browser`. `@wxt-dev/browser` prefers
// `globalThis.browser` whenever it carries a `runtime.id`, and so does the
// extension's own store port — so a harness that claimed both names would win
// over a suite's own fake and, worse, would make the "Chrome shape, no browser
// global" case untestable. Occupying only `chrome` keeps that case honest: a
// suite that stubs neither name still gets a catalog, and a suite that
// deliberately runs without `browser` still sees exactly what it meant to see.
const existing = g.chrome as { i18n?: { getMessage?: unknown } } | undefined;
if (typeof existing?.i18n?.getMessage !== 'function') {
  g.chrome = {
    runtime: { id: 'mock-extension-id', ...runtimeApi(TEST_EXTENSION_ORIGIN) },
    i18n: i18nApi('en'),
  };
}

// Node has a real `fetch`; only the extension's own catalog URLs are
// intercepted, and the real one still handles everything else. A test that
// wants the read to fail replaces this with `catalogFetch({ fail: true })`.
/**
 * 🔴 W18 · **A fresh, empty IndexedDB for every test**, because the backfill debt
 *    set now lives there (`lib/backfill/debt-store.ts`) alongside the outbox.
 *
 * Why here and not in each suite: in the browser IndexedDB is simply there — the
 * manifest grants `unlimitedStorage` and the outbox has depended on it since W2 —
 * so a suite that had to opt in would be testing a configuration the extension
 * never runs in. Installing it once means "no IndexedDB" is the case a suite has
 * to *say*, exactly as it already does with `delete globalThis.indexedDB`, rather
 * than the silent default.
 *
 * A *fresh* factory per test, not one shared one: the old layout put each test's
 * debt set in its own `memoryStore()`, and a shared database would let one test's
 * debts leak into the next one's assertions — a failure that reads like a logic
 * bug. Suites that install their own factory in their own `beforeEach` still win,
 * because a file's `beforeEach` hooks run after these.
 *
 * Assignment rather than `vi.stubGlobal`, for the same reason as the `chrome`
 * shim above: suites call `vi.unstubAllGlobals()`, and this is not what they are
 * testing.
 */
beforeEach(() => {
  (globalThis as Record<string, unknown>).indexedDB = new IDBFactory();
});

const realFetch = (globalThis.fetch as typeof fetch).bind(globalThis);
const serveCatalog = catalogFetch();
globalThis.fetch = ((input: unknown, init?: unknown) => {
  const url = String(input);
  if (url.startsWith(TEST_EXTENSION_ORIGIN) && url.endsWith('/messages.json')) {
    return serveCatalog(url) as unknown as Promise<Response>;
  }
  return realFetch(input as RequestInfo, init as RequestInit);
}) as typeof fetch;
