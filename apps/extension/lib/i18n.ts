/**
 * The translation seam for every string the extension shows a user.
 *
 * ## Why there is a layer here at all
 *
 * `@wxt-dev/i18n` compiles `locales/<locale>.yml` into the standard
 * `_locales/<locale>/messages.json` catalog and wraps `browser.i18n.getMessage`
 * in a typed `t` (node_modules/@wxt-dev/i18n/dist/index.mjs:4-40). Its own
 * README states the limitation this file exists to work around, in the same
 * words as the underlying browser API:
 *
 *   "Like the `browser.i18n` API, to change the language, users must change the
 *    browser's language"
 *   — node_modules/@wxt-dev/i18n/README.md, "Downside" section
 *
 * So `i18n.t` cannot be talked into returning Chinese to a user whose browser
 * is English. The popup needs a switch, and the switch has to be honest about
 * what it does, so:
 *
 *   • `auto`   — hand the call straight to `i18n.t`. This is the standard path
 *                and the one that keeps working with no extra machinery.
 *   • `en` / `zh_CN` — read the catalog the extension already ships
 *                (`_locales/<code>/messages.json`, the very same file the
 *                browser would have read) and do the substitution here.
 *
 * ## Why the substitution is re-implemented instead of reusing `i18n.t`
 *
 * It is not re-implemented: it is *mirrored*, rule for rule, from the package,
 * and it deliberately supports the same three argument shapes so that a call
 * site does not have to know which path it is on. The rules, with their source:
 *
 *   • how the variadic arguments are classified (number = plural count, array =
 *     positional substitutions, object = named substitutions, anything else is
 *     an error) — dist/index.mjs:9-14;
 *   • `count` alone becomes `$1` for the positional pass — dist/index.mjs:15;
 *   • positional `$1`-`$9` are the browser's job — dist/index.mjs:17-20;
 *   • the plural split on `" | "` with the 1/2/3-part switch — dist/index.mjs:22-36;
 *   • named `{name}` replacement — dist/utils-C75KGsJ7.mjs:2,9-13.
 *
 * This catalog only ever uses `{name}`, precisely so that both paths agree by
 * construction rather than by luck; a test pins that no entry uses `$n`
 * (tests/i18n-catalog.test.ts). The positional branch is still implemented and
 * tested, because if it existed in the package and not here, a future entry
 * using `$1` would silently render `$1` to a user who picked a language.
 *
 * ## Falling back is not silent
 *
 * If the catalog cannot be read, or an entry is missing from it, the call goes
 * to `i18n.t` — and that is exactly the sort of "we do not know" this project
 * refuses to swallow. The first such fallback logs a `console.warn` naming the
 * locale and the key. It warns once per locale selection rather than once per
 * lookup, so a missing entry does not turn every repaint into noise, but it
 * never becomes silent.
 *
 * ## Sync by design
 *
 * `t` is synchronous. The catalog is fetched once by `initUiLocale` and cached,
 * which is the only way a pure render function (`renderPopup`) and a badge
 * title setter can both stay synchronous. Until the catalog has landed, `t`
 * behaves exactly like the `auto` path — so the worst case is the pre-existing
 * behaviour, never a blank string.
 */

import { createI18n } from '@wxt-dev/i18n';

/**
 * The package's own translator.
 *
 * The WXT module also generates a typed copy behind the `#i18n` alias
 * (`.wxt/i18n/index.ts`, node_modules/@wxt-dev/i18n/dist/module.mjs:69-80), and
 * that is the documented way to import it. It cannot be used here: the file
 * only exists once `wxt prepare` has finished, while `wxt prepare` already
 * loads the entrypoints that import this module. On a fresh clone that is a
 * cycle, and `pnpm install` fails with "Cannot find module '#i18n'".
 *
 * The generated file is exactly `createI18n<GeneratedI18nStructure>()`. The
 * type parameter is erased at runtime and `t` is cast to an untyped signature
 * below anyway, so this is the same object without the cycle.
 */
const i18n = createI18n();

/** Where the popup's Language selector stores its choice. */
export const UI_LOCALE_KEY = 'cs_ui_locale';

/** The three values the setting may take. `auto` follows the browser. */
export const UI_LOCALES = ['auto', 'en', 'zh_CN'] as const;
export type UiLocale = (typeof UI_LOCALES)[number];

/**
 * 🔴 `auto` is the default, and it must stay the default: it is the only value
 * that behaves the way a browser extension is expected to behave, and a user
 * who never opens the selector should get their browser's language.
 */
export const DEFAULT_UI_LOCALE: UiLocale = 'auto';

/** The locales `en`/`zh_CN` name catalogs for. `auto` has no catalog of ours. */
export type OverrideLocale = Exclude<UiLocale, 'auto'>;

/** Anything unrecognised (a stale value, a hand-edited one) means `auto`. */
export function normalizeUiLocale(value: unknown): UiLocale {
  return typeof value === 'string' && (UI_LOCALES as readonly string[]).includes(value)
    ? (value as UiLocale)
    : DEFAULT_UI_LOCALE;
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

interface CatalogEntry {
  message?: string;
  placeholders?: Record<string, { content?: string }>;
}
type Catalog = Record<string, CatalogEntry>;

type Sub = string | number;
type PositionalSubs = readonly Sub[];
type NamedSubs = Record<string, Sub>;

/**
 * Where the compiled catalog lives inside the extension package. The path is
 * the standard one; the same file is what the browser itself reads for
 * `browser.i18n`, so nothing extra has to be shipped or declared.
 */
export function catalogPath(locale: OverrideLocale): string {
  return `/_locales/${locale}/messages.json`;
}

interface ExtensionBits {
  runtime?: { getURL?: (path: string) => string };
}

function extensionRuntime(): { getURL?: (path: string) => string } | null {
  const g = globalThis as { browser?: ExtensionBits; chrome?: ExtensionBits };
  return g.browser?.runtime ?? g.chrome?.runtime ?? null;
}

/**
 * Read one compiled catalog. Returns null on *any* failure — no URL, no fetch,
 * a non-OK response, unparseable JSON — because every one of those means the
 * same thing to the caller: we do not have the text, fall back.
 */
async function loadCatalog(locale: OverrideLocale): Promise<Catalog | null> {
  try {
    const url = extensionRuntime()?.getURL?.(catalogPath(locale));
    if (!url) return null;
    const response = await fetch(url);
    if (response && typeof response.ok === 'boolean' && !response.ok) return null;
    const parsed: unknown = await response.json();
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
    return parsed as Catalog;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Module state
// ---------------------------------------------------------------------------

const state: { locale: UiLocale; catalog: Catalog | null; warned: boolean } = {
  locale: DEFAULT_UI_LOCALE,
  catalog: null,
  warned: false,
};

/** The locale the overlay is currently rendering in. `auto` means "the package". */
export function currentUiLocale(): UiLocale {
  return state.locale;
}

function warnOnce(message: string): void {
  if (state.warned) return;
  state.warned = true;
  console.warn(`[chat-stasher] i18n: ${message}`);
}

/** Switch the overlay's locale and load its catalog. Does not touch storage. */
export async function applyUiLocale(locale: UiLocale): Promise<UiLocale> {
  state.locale = normalizeUiLocale(locale);
  state.catalog = null;
  state.warned = false;
  if (state.locale === 'auto') return state.locale;
  const catalog = await loadCatalog(state.locale);
  if (catalog === null) {
    warnOnce(
      `could not read ${catalogPath(state.locale)}; falling back to browser.i18n `
      + `for the rest of this selection`,
    );
    return state.locale;
  }
  state.catalog = catalog;
  return state.locale;
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

interface LocaleStorage {
  get(query: Record<string, unknown>): Promise<Record<string, unknown>>;
  set(values: Record<string, unknown>): Promise<void>;
}

/** `browser.storage.local` (or its `chrome` alias), or null when unavailable. */
function localeStorage(): LocaleStorage | null {
  const g = globalThis as {
    browser?: { storage?: { local?: LocaleStorage } };
    chrome?: { storage?: { local?: LocaleStorage } };
  };
  const local = g.browser?.storage?.local ?? g.chrome?.storage?.local;
  return local && typeof local.get === 'function' && typeof local.set === 'function' ? local : null;
}

/**
 * Read the stored choice. A storage that is missing or throwing answers `auto`
 * — the same answer as "never chosen", which is the only honest reading: we
 * cannot claim the user picked a language we could not read.
 */
export async function loadUiLocale(): Promise<UiLocale> {
  try {
    const storage = localeStorage();
    if (!storage) return DEFAULT_UI_LOCALE;
    // An object query, not `get([key])`: this is the documented shape that
    // returns the caller's default when the key is absent, and it means the
    // absence of the setting and the absence of a value cannot be confused.
    const values = await storage.get({ [UI_LOCALE_KEY]: DEFAULT_UI_LOCALE });
    return normalizeUiLocale(values?.[UI_LOCALE_KEY]);
  } catch {
    return DEFAULT_UI_LOCALE;
  }
}

/** Persist the choice, then apply it. Returns what actually ended up applied. */
export async function setUiLocale(locale: UiLocale): Promise<UiLocale> {
  const wanted = normalizeUiLocale(locale);
  try {
    await localeStorage()?.set({ [UI_LOCALE_KEY]: wanted });
  } catch (err) {
    // The UI still switches for this session; next time it will read back the
    // old value. Saying so is better than pretending the write landed.
    console.warn('[chat-stasher] i18n: could not persist the language choice', (err as Error).message);
  }
  return applyUiLocale(wanted);
}

/** Read the stored choice and load the matching catalog. Call once per context. */
export async function initUiLocale(): Promise<UiLocale> {
  return applyUiLocale(await loadUiLocale());
}

// ---------------------------------------------------------------------------
// Substitution — mirrored from @wxt-dev/i18n, see the header for citations
// ---------------------------------------------------------------------------

/**
 * Chrome's `$n` substitution, which is what the package delegates to by calling
 * `browser.i18n.getMessage(key, subs)`. `$$` is a literal `$`; `$1`-`$9` are the
 * substitutions; a `$` that starts neither is left alone. Substitutions are
 * 1-indexed and an out-of-range index becomes the empty string, matching
 * https://developer.chrome.com/docs/extensions/reference/api/i18n#type-MessageFormatter
 */
function applyPositional(message: string, subs: PositionalSubs): string {
  return message.replace(/\$(\$|[1-9])/g, (_match, token: string) => {
    if (token === '$') return '$';
    return String(subs[Number(token) - 1] ?? '');
  });
}

/**
 * `{name}` replacement — the same regex and the same "leave unknown names
 * alone" rule as dist/utils-C75KGsJ7.mjs:2,9-13.
 */
function applyNamedSubstitutions(message: string, subs: NamedSubs): string {
  return message.replace(/\{([A-Za-z0-9_]+)\}/g, (match, key: string) =>
    Object.prototype.hasOwnProperty.call(subs, key) ? String(subs[key]) : match);
}

/** The plural switch, verbatim from dist/index.mjs:22-36. */
function applyPlural(message: string, count: number): string {
  const plural = message.split(' | ');
  switch (plural.length) {
    case 1:
      return plural[0]!;
    case 2:
      return plural[count === 1 ? 0 : 1]!;
    case 3:
      return plural[count === 0 || count === 1 ? count : 2]!;
    default:
      throw new Error('Unknown plural formatting');
  }
}

interface ClassifiedArgs {
  count: number | null;
  positional: PositionalSubs | null;
  named: NamedSubs | null;
}

/** Argument classification, mirrored from dist/index.mjs:9-14. */
function classifyArgs(args: readonly unknown[]): ClassifiedArgs {
  let count: number | null = null;
  let positional: PositionalSubs | null = null;
  let named: NamedSubs | null = null;
  args.forEach((arg, i) => {
    if (arg == null) return;
    if (typeof arg === 'number') count = arg;
    else if (Array.isArray(arg)) positional = arg as PositionalSubs;
    else if (typeof arg === 'object') named = arg as NamedSubs;
    else {
      throw new Error(
        `Unknown argument at index ${i}. Must be a number for pluralization, `
        + 'substitution array, or named substitution object.',
      );
    }
  });
  return { count, positional, named };
}

/** Render one catalog template with the caller's arguments. */
export function renderMessage(
  template: string,
  args: readonly unknown[],
): string {
  const { count, positional, named } = classifyArgs(args);
  // dist/index.mjs:15 — a bare count is also the first positional substitution.
  const subs = positional ?? (count !== null ? [String(count)] : null);
  let message = template;
  if (subs && subs.length > 0) message = applyPositional(message, subs);
  if (count !== null) message = applyPlural(message, count);
  if (named) message = applyNamedSubstitutions(message, named);
  return message;
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

type UntypedT = (key: string, ...args: unknown[]) => string;

/**
 * The package's own `t`, used for `auto` and as the fallback for everything
 * else. Cast because the generated types are exact about key names and
 * substitution shapes, while this module takes keys that only exist at runtime
 * (the per-platform coverage notes are looked up by a key stored in a table).
 */
const packageT = i18n.t as unknown as UntypedT;

/**
 * Translate `key`, substituting `args`.
 *
 * Keys are dotted (`popup.running.active`); the catalog is flat, keyed with
 * underscores, exactly as the package looks them up
 * (`key.replaceAll(".", "_")`, dist/index.mjs:19-20).
 */
export function t(key: string, ...args: unknown[]): string {
  const entry = state.catalog?.[key.replaceAll('.', '_')];
  if (typeof entry?.message === 'string' && entry.message.length > 0) {
    return renderMessage(entry.message, args);
  }
  // Only an override can get here with a catalog loaded: in `auto` the package
  // owns the lookup, and it warns on its own for a missing message
  // (dist/index.mjs:21). Warn here too rather than quietly showing the other
  // language — a half-translated screen must be visible to whoever is looking.
  if (state.locale !== 'auto') {
    warnOnce(`no entry "${key}" in ${catalogPath(state.locale)}; using browser.i18n for it`);
  }
  return packageT(key, ...args);
}
