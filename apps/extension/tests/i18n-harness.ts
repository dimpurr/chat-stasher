/**
 * The translation stand-in the test suite runs against.
 *
 * It is deliberately built from the *real* catalog files and through the
 * *package's own* compiler, so a test that asserts English text is asserting
 * what the shipped extension would say:
 *
 *   locales/en.yml --(@wxt-dev/i18n/build parse+generate)--> the same
 *   `Record<key, {message}>` document that lands in `_locales/en/messages.json`.
 *
 * From there two things are faked, and only two:
 *
 *   1. `browser.i18n.getMessage`, because a Node process has no browser. It
 *      implements Chrome's `$1`-`$9` / `$$` substitution, which is what the
 *      extension's English path delegates to — see the citation in lib/i18n.ts.
 *   2. `fetch` for the catalog URLs, so the overlay's own read path can be
 *      exercised (including its failure modes: see `catalogFetch({ fail: true })`).
 *
 * Nothing here is a second copy of any wording: every string comes out of the
 * yml files, so the harness cannot drift from what ships.
 */

import { resolve } from 'node:path';
import {
  generateChromeMessages,
  parseMessagesFile,
  type ChromeMessage,
} from '@wxt-dev/i18n/build';

export const TEST_LOCALES = ['en', 'zh_CN'] as const;
export type TestLocale = (typeof TEST_LOCALES)[number];

/** The origin every fake `runtime.getURL` in this suite builds URLs on. */
export const TEST_EXTENSION_ORIGIN = 'chrome-extension://mock-extension-id';

export type TestCatalog = Record<string, ChromeMessage>;

async function loadCatalogs(): Promise<Record<TestLocale, TestCatalog>> {
  const catalogs = {} as Record<TestLocale, TestCatalog>;
  for (const locale of TEST_LOCALES) {
    // 🔴 Resolved from `__dirname`, not `import.meta.url`. Inside `setupFiles`, a
    // suite that asks for a DOM environment gets an `import.meta.url` on the
    // document's origin (measured under both jsdom and happy-dom:
    // `http://localhost:3000/`), so a URL built from it points at a path that
    // does not exist and the whole file fails before its first test. `__dirname`
    // is the file's own directory in every environment vitest runs, so this
    // reads the same two catalog files either way.
    const file = resolve(__dirname, '..', 'locales', `${locale}.yml`);
    catalogs[locale] = generateChromeMessages(await parseMessagesFile(file));
  }
  return catalogs;
}

export const CATALOGS = await loadCatalogs();

/**
 * Chrome's substitution rule for `browser.i18n.getMessage`:
 * `$$` is a literal `$`, `$1`-`$9` are the substitutions, and anything else
 * after a `$` is left alone. Substitutions are 1-indexed.
 * https://developer.chrome.com/docs/extensions/reference/api/i18n#type-MessageFormatter
 */
function substitute(template: string, subs: readonly string[]): string {
  return template.replace(/\$(\$|[1-9])/g, (_match, token: string) =>
    token === '$' ? '$' : String(subs[Number(token) - 1] ?? ''));
}

/** A `browser.i18n` that answers from one compiled catalog. */
export function i18nApi(locale: TestLocale = 'en'): { getMessage: (key: string, subs?: string | string[]) => string } {
  const catalog = CATALOGS[locale];
  return {
    getMessage(key: string, substitutions?: string | string[]): string {
      const entry = catalog[key];
      // Chrome returns the empty string for a key it does not know; the package
      // then warns (dist/index.mjs:21). Reproducing that is the point.
      if (!entry) return '';
      const subs = substitutions == null
        ? []
        : (Array.isArray(substitutions) ? substitutions : [substitutions]);
      return substitute(entry.message, subs);
    },
  };
}

/**
 * 🔴 W59 · The version every fake extension API in this suite reports, i.e. "the
 * build running now" for a test that does not say otherwise.
 *
 * Why the harness has one at all: in a real browser `runtime.getManifest()`
 * always answers, and a halt record's build stamp is read from it. A harness
 * without one would make every suite run the *degraded* configuration — a build
 * that cannot name itself — and the W59 rule would then be inert in exactly the
 * place it is supposed to be exercised (the same shape of hole as a test that
 * disables the cache the product runs with). Suites that want "cannot name
 * itself" delete this member on their own fake, which is a fact they then state.
 */
export const TEST_BUILD_ID = '0.1.0.1';

/** The `runtime` bits the overlay needs to locate a catalog — plus the manifest, as in a real browser. */
export function runtimeApi(origin: string = TEST_EXTENSION_ORIGIN): {
  getURL: (path: string) => string;
  getManifest: () => { version: string };
} {
  return {
    getURL: (path: string) => `${origin}${path.startsWith('/') ? '' : '/'}${path}`,
    getManifest: () => ({ version: TEST_BUILD_ID }),
  };
}

export interface CatalogFetchOptions {
  /** Make every catalog read fail, so the fallback path can be exercised. */
  fail?: boolean;
}

const NOT_FOUND = {
  ok: false,
  status: 404,
  async json(): Promise<never> {
    throw new Error('catalog fetch: not found');
  },
};

/**
 * A `fetch` stand-in that serves the extension's own compiled catalogs, and
 * refuses everything else loudly rather than quietly reaching the network.
 */
export function catalogFetch(options: CatalogFetchOptions = {}) {
  return async (input: unknown): Promise<{ ok: boolean; status: number; json: () => Promise<unknown> }> => {
    const url = String(input);
    if (options.fail) return NOT_FOUND;
    const match = /\/_locales\/([^/]+)\/messages\.json$/.exec(url);
    const locale = match?.[1] as TestLocale | undefined;
    if (!locale || !(locale in CATALOGS)) return NOT_FOUND;
    return { ok: true, status: 200, async json() { return CATALOGS[locale]; } };
  };
}

interface BrowserLike {
  runtime?: Record<string, unknown>;
}

/**
 * Give a suite's fake browser the two members the translation layer reaches
 * for. `@wxt-dev/browser` captures `globalThis.browser` when it is first
 * imported (node_modules/@wxt-dev/browser/src/index.mjs), so a suite that
 * replaces the whole object has to carry `i18n` with it or the English path
 * throws. Nothing else about the fake browser is touched.
 */
export function withI18n<T extends BrowserLike>(
  browser: T,
  locale: TestLocale = 'en',
): T & { i18n: ReturnType<typeof i18nApi> } {
  // Properties are assigned onto the object that was handed in, and its `runtime`
  // object is extended in place rather than replaced. Identity matters here: a
  // suite keeps mutating its own fake (lastError, sendNativeMessage, badgeText)
  // after stubbing, and anything that copied the object would stop seeing those
  // mutations — which is a test that passes for the wrong reason.
  (browser as { i18n?: unknown }).i18n = i18nApi(locale);
  if (browser.runtime == null) browser.runtime = {};
  Object.assign(browser.runtime, runtimeApi());
  return browser as T & { i18n: ReturnType<typeof i18nApi> };
}
