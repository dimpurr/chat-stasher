/**
 * Build-number-aware manifest versioning.
 *
 * Why it exists: real-browser acceptance runs against an unpacked build, and
 * Chrome only re-reads a manifest when the version changes — its "Update"
 * button and the reload arrow do not re-inject content scripts. Every dev
 * reload cycle therefore bumps the 4th version component, which Chrome treats
 * as a new version and so re-runs `onInstalled` → re-injects the content
 * scripts. `version_name` carries the same bump as a human-readable
 * `<semver>+build.<n>` so the loaded build is identifiable in
 * chrome://extensions without decoding the numeric version.
 *
 * When no build number is supplied (`CS_BUILD_NUMBER` unset or empty) the
 * version is left exactly as it has always been — the plain semver from
 * package.json and no `version_name` key — so an ordinary build produces a
 * byte-identical manifest. The manifest merge in `wxt.config.ts` relies on
 * that property, and `tests/build-version.test.ts` pins it.
 */

export interface VersionResult {
  /** The value for `manifest.version`. */
  version: string;
  /** The value for `manifest.version_name`, present only when a build number is supplied. */
  versionName?: string;
}

/**
 * Accepts `0` and any number of digits after a non-zero leading digit. A
 * leading zero like `007` is rejected because as a build number it is
 * ambiguous; the caller gets a clear error rather than a guess.
 */
const BUILD_NUMBER_RE = /^(0|[1-9][0-9]*)$/;

/**
 * Parses `CS_BUILD_NUMBER`. Returns `undefined` when it is unset or empty,
 * meaning "no build number" (byte-identical manifest). Throws a clear error
 * for anything that is not a non-negative integer.
 */
export function parseBuildNumber(raw: string | undefined): number | undefined {
  if (raw === undefined || raw === '') return undefined;
  if (!BUILD_NUMBER_RE.test(raw)) {
    throw new Error(
      `CS_BUILD_NUMBER must be a non-negative integer, got ${JSON.stringify(raw)}`,
    );
  }
  return Number(raw);
}

/**
 * Maps a base semver and an optional build number onto the manifest version
 * fields. Without a build number the base semver is returned unchanged and no
 * `versionName` is produced, which keeps the manifest byte-identical to an
 * unversioned build.
 */
export function buildVersion(semver: string, buildNumber: number | undefined): VersionResult {
  if (buildNumber === undefined) return { version: semver };
  return {
    version: `${semver}.${buildNumber}`,
    versionName: `${semver}+build.${buildNumber}`,
  };
}

/**
 * 🔴 W59b · **The stamp that tells one build from another when the manifest version
 * cannot.**
 *
 * Why this exists next to `buildVersion` rather than inside it: the two answer
 * different questions, and W59 needed the second one. `buildVersion` produces the
 * *manifest* version, which is what Chrome compares to decide whether to re-inject
 * content scripts — so it has to be a value a human can plan a release around, and it
 * is bumped only when someone runs the dev reload script. That leaves every other
 * build of the extension wearing the same `0.1.0`, and a halt record stamped `0.1.0`
 * by one build was read by the next as its own judgement (lib/extension-build.ts).
 *
 * This stamp is not a release number and claims nothing about compatibility; it is
 * "which build is this", baked into the bundle by the bundler
 * (`vite.define` in wxt.config.ts). Two builds produced from different source cannot
 * share it. The `b` prefix and base-36 body make it impossible to read as a semver —
 * or as the `<semver>.<n>` form above — which is what keeps a reader (and the popup's
 * wording) able to tell the two halves of an identity apart.
 *
 * 🔴 Takes `now` rather than reading the clock, so the one property that matters here
 *    — distinct milliseconds give distinct stamps — can be asserted without waiting.
 */
export function buildStamp(nowMs: number): string {
  return `b${Math.trunc(nowMs).toString(36)}`;
}