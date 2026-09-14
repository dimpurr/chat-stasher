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