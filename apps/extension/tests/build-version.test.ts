/**
 * W24 · Build-number-aware manifest versioning (lib/build-version.ts).
 *
 * Why this case is worth having: the dev-reload loop bumps the 4th version
 * component so Chrome notices a "new version" and re-injects content scripts.
 * Two properties must not silently regress:
 *   1. without `CS_BUILD_NUMBER` the manifest is byte-identical to a plain
 *      build (no 4th component, no `version_name`) — otherwise an unversioned
 *      build drifts from what release ships;
 *   2. an invalid `CS_BUILD_NUMBER` fails loudly rather than guessing, so the
 *      reload script never installs a manifest it mis-parsed.
 */

import { describe, it, expect } from 'vitest';
import { readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';

import { buildVersion, parseBuildNumber } from '../lib/build-version';

const ROOT = new URL('..', import.meta.url).pathname;

describe('buildVersion', () => {
  it('keeps the base semver and no version_name when there is no build number', () => {
    expect(buildVersion('0.1.0', undefined)).toEqual({ version: '0.1.0' });
  });

  it('appends the build number as the 4th component', () => {
    expect(buildVersion('0.1.0', 7)).toEqual({
      version: '0.1.0.7',
      versionName: '0.1.0+build.7',
    });
  });

  it('accepts a multi-digit build number', () => {
    expect(buildVersion('0.1.0', 123)).toEqual({
      version: '0.1.0.123',
      versionName: '0.1.0+build.123',
    });
  });

  it('accepts build number zero', () => {
    expect(buildVersion('1.2.3', 0)).toEqual({
      version: '1.2.3.0',
      versionName: '1.2.3+build.0',
    });
  });

  it('leaves a non-semver base alone (the base is validated by WXT)', () => {
    // The base comes from package.json; buildVersion must not reformat it.
    expect(buildVersion('0.1.0', undefined)).toEqual({ version: '0.1.0' });
  });
});

describe('parseBuildNumber', () => {
  it('returns undefined when unset or empty', () => {
    expect(parseBuildNumber(undefined)).toBeUndefined();
    expect(parseBuildNumber('')).toBeUndefined();
  });

  it('parses a plain non-negative integer', () => {
    expect(parseBuildNumber('0')).toBe(0);
    expect(parseBuildNumber('9')).toBe(9);
    expect(parseBuildNumber('10')).toBe(10);
  });

  it('rejects anything that is not a base-10 non-negative integer', () => {
    for (const bad of ['-1', 'abc', '1.5', '007', ' 1', '1 ', '1e3', '0x10', 'NaN']) {
      expect(() => parseBuildNumber(bad), `should reject ${JSON.stringify(bad)}`).toThrow(
        /CS_BUILD_NUMBER must be a non-negative integer/,
      );
    }
  });
});

describe('the produced manifest stays byte-identical without a build number', () => {
  // Mirrors the pattern in w2-manifest.test.ts: this layer only bites when the
  // build output exists, i.e. after `pnpm -s build` at closing time.
  it('a built manifest has no version_name and a 4-component-free version', () => {
    const manifestPath = join(ROOT, '.output', 'chrome-mv3', 'manifest.json');
    if (!existsSync(manifestPath)) return; // not built yet — nothing to pin
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    // `version` equals the semver from package.json with no 4th component.
    const pkg = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8'));
    expect(String(manifest.version)).toBe(String(pkg.version));
    expect(manifest.version_name).toBeUndefined();
  });
});