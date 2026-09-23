/**
 * 🔴 W59 · **Which extension build is running**, as the browser itself names it.
 *
 * Why a module of its own rather than a constant in the file that uses it: two
 * callers need the same answer and must not be able to compute two —
 * `lib/backfill/engine.ts` stamps it on the halts it writes, and
 * `entrypoints/background.ts`'s `scopeRetryDue` judges stored records against it
 * *before* any run happens. A second way of spelling "the current build" (a
 * hard-coded number, a `package.json` import) is exactly the drift W44 refused
 * for the capability marker: it fails silently, and it fails in the direction of a
 * record outliving the truth it carries.
 *
 * The value is `runtime.getManifest().version`. With no `CS_BUILD_NUMBER` that is
 * the plain semver from `package.json`; with one it is `<semver>.<n>`, bumped on
 * every dev reload (W24, lib/build-version.ts) — so a reloaded build and the one
 * it replaced are told apart, which is the whole point. Chrome only re-reads a
 * manifest when the version changes, so the identity that makes the browser
 * re-inject content scripts is the same identity that makes a stale halt expire,
 * and neither can move without the other.
 *
 * 🔴 **null means "this build cannot name itself", and it is not "no build".** The
 *    reader answers it conservatively (a record is never cleared on the strength
 *    of a guess — see `HaltJudgement` in lib/backfill/types.ts), so an environment
 *    without the API gets the old behaviour exactly rather than a new one nobody
 *    asked for. It is read through the same shim test lib/backfill/store.ts uses,
 *    because Chrome MV3 has no `browser` global and Firefox has no `chrome` one.
 */

type ExtApi = { runtime?: { id?: string; getManifest?: () => unknown } };

/** The same test WXT's own shim uses, so this cannot disagree with the rest of the build. */
function extensionApi(): ExtApi | null {
  const g = globalThis as { browser?: ExtApi; chrome?: ExtApi };
  if (g.browser?.runtime?.id) return g.browser;
  if (g.chrome?.runtime?.id) return g.chrome;
  return g.browser ?? g.chrome ?? null;
}

/**
 * The running extension's version, or null when it cannot be read.
 *
 * 🔴 An empty or non-string version is **null**, never `''`: a stamp of `''` would
 *    compare equal to another `''` and would read as "the same build" — a record
 *    held in force by a value that means "we could not tell". The reader refuses
 *    to clear a record on an unnamed build, so returning null here loses nothing
 *    and invents nothing.
 */
export function runningBuildId(): string | null {
  const api = extensionApi();
  if (!api || typeof api.runtime?.getManifest !== 'function') return null;
  let manifest: unknown;
  try {
    manifest = api.runtime.getManifest();
  } catch {
    // A manifest we cannot read is a fact to report as "unknown", not an error to
    // throw into a run: the run's own work is unaffected by losing this stamp.
    return null;
  }
  const version = (manifest as { version?: unknown } | null)?.version;
  return typeof version === 'string' && version.length > 0 ? version : null;
}
