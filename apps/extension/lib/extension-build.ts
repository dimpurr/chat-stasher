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
 * 🔴 🔴 W59b · **The manifest version alone was not an identity, and this is the
 *    defect that replaced it.** `runtime.getManifest().version` is `<semver>.<n>`
 *    **only when `CS_BUILD_NUMBER` is set**, and nothing sets it except
 *    `scripts/dev/reload-extension.sh` (W24, lib/build-version.ts). Chrome's own
 *    Reload button, `wxt dev`, and a plain `pnpm build` all produce `0.1.0` — so two
 *    builds with completely different source shared one identity, a halt written by
 *    the first was read by the second as *its own* judgement, and W59's re-decision
 *    silently never fired. An identity every build shares is not an identity.
 *
 *    So the version is composed with a **build stamp baked in at build time**
 *    (`__CS_BUILD_STAMP__`, defined by `vite.define` in wxt.config.ts, and by the
 *    same key in vitest.config.ts so the suite does not run the degraded
 *    configuration). Two builds of different source cannot share it, whatever the
 *    manifest version says; and the version is kept in the string rather than
 *    replaced, because a human reading a halt record needs to know both which
 *    release and which build.
 *
 *    🔴 **What two loads still share it, stated rather than implied: the same built
 *       bundle.** Re-opening the browser, and Chrome's Reload button on the same
 *       `.output/chrome-mv3`, do not re-run the build — the bytes on disk are
 *       unchanged, so it *is* the same build and it should read as one. A `wxt dev`
 *       session that recompiles on a file change is the one place a new bundle can
 *       appear without the stamp moving, because the stamp is folded from the config
 *       that session started with; the documented reload loop
 *       (`scripts/dev/reload-extension.sh`) does move it, since it runs a fresh
 *       build, and it moves the manifest version as well.
 *
 * 🔴 **null means "this build cannot name itself", and it is not "no build".** The
 *    reader answers it conservatively (a record is never cleared on the strength
 *    of a guess — see `HaltJudgement` in lib/backfill/types.ts), so an environment
 *    without the API gets the old behaviour exactly rather than a new one nobody
 *    asked for. It is read through the same shim test lib/backfill/store.ts uses,
 *    because Chrome MV3 has no `browser` global and Firefox has no `chrome` one.
 */

/**
 * 🔴 W59b · **The build stamp, replaced at build time by the bundler.**
 *
 * `typeof` rather than a bare read, and this is the one form that is safe on an
 * identifier the bundler did not replace: `typeof undeclaredName` is the single
 * expression in JavaScript that answers `'undefined'` instead of throwing, so an
 * unpackaged run (a unit test importing this module directly, a `tsc`-only build of
 * the library) reads as "no stamp" rather than failing to load. The `declare` is what
 * lets `tsc --noEmit` compile the read in the first place.
 *
 * The value is deliberately **not** a version-shaped string — see `composeBuildId`.
 */
declare const __CS_BUILD_STAMP__: string | undefined;

type ExtApi = { runtime?: { id?: string; getManifest?: () => unknown } };

/** The same test WXT's own shim uses, so this cannot disagree with the rest of the build. */
function extensionApi(): ExtApi | null {
  const g = globalThis as { browser?: ExtApi; chrome?: ExtApi };
  if (g.browser?.runtime?.id) return g.browser;
  if (g.chrome?.runtime?.id) return g.chrome;
  return g.browser ?? g.chrome ?? null;
}

/**
 * The build stamp the bundler baked in, or null when there is none.
 *
 * 🔴 Empty is null, for the same reason an empty version is: `''` would compose into
 *    a string that compares equal to another build's and would read as "the same
 *    build" — a record held in force by a value that means "there was no stamp".
 */
export function bakedBuildStamp(): string | null {
  return typeof __CS_BUILD_STAMP__ === 'string' && __CS_BUILD_STAMP__.length > 0
    ? __CS_BUILD_STAMP__
    : null;
}

/**
 * 🔴 W59b · **Compose the identity a halt record is stamped with, from the two facts
 * that make it up.**
 *
 * Pure and exported because this is the function the fix is *about*: "does a record
 * from another build still apply" is answered by comparing two of these, and the
 * property that W59 needed and did not have — two builds that report the same
 * manifest version do not compare equal — is a property of this composition, testable
 * without a browser. See tests/w59b-stale-halt-retry.test.ts.
 *
 * The stamp is appended with `+` and never substituted for the version, so the two
 * facts stay separable in a record, a log line and the popup: `0.1.0+bmtq3x1f` is
 * "release 0.1.0, build bmtq3x1f". A `null` stamp composes to the bare version, which
 * is exactly what every record written before this change looks like and what a
 * stamp-less environment must keep producing.
 *
 * 🔴 The stamp is a `b`-prefixed base-36 number, and that shape is deliberate: it
 *    cannot be mistaken for a semver (or for the `<semver>.<n>` form
 *    `CS_BUILD_NUMBER` produces), so a reader — or a future change that wants to
 *    split the two apart again — cannot confuse it with the version half.
 */
export function composeBuildId(version: string, stamp: string | null): string {
  return stamp === null ? version : `${version}+${stamp}`;
}

/**
 * The running extension's identity, or null when it cannot be read.
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
  if (typeof version !== 'string' || version.length === 0) return null;
  return composeBuildId(version, bakedBuildStamp());
}
