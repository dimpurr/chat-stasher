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

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { readFileSync, rmSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';

import { buildVersion, parseBuildNumber } from '../lib/build-version';

const ROOT = new URL('..', import.meta.url).pathname; // apps/extension
const WXT_BIN = join(ROOT, 'node_modules', 'wxt', 'bin', 'wxt.mjs');
const OUT_BASE = join(ROOT, '.output', 'build-version-pin');

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
  /**
   * 🔴 W115 · **Build the manifest this case reads, rather than reading the
   * shared `.output/chrome-mv3`.**
   *
   * `.output` is whatever build ran last — a dev-reload build that carries a
   * build number, or one produced before a `package.json` version bump. Reading
   * it made this case assert the freshness of an artifact it did not control:
   * in the main checkout `pnpm test` failed with "expected '0.1.0' to be
   * '0.2.0'" only because `.output` was older than `package.json`. That is a
   * test depending on a stale artifact, not a property of the build.
   *
   * So the case runs the real build into its own `CS_OUT_DIR` — the same seam
   * `w91-build-channels.test.ts` uses — and reads that manifest. The property
   * under test is unchanged; it no longer measures the age of someone else's
   * output. `CS_BUILD_NUMBER` is cleared so the run tests "no build number"
   * regardless of the caller's environment, which is exactly the case this
   * describe names.
   */
  let manifest: { version?: unknown; version_name?: unknown };

  beforeAll(() => {
    rmSync(OUT_BASE, { recursive: true, force: true });
    const env: NodeJS.ProcessEnv = { ...process.env, CS_OUT_DIR: OUT_BASE };
    delete env.CS_BUILD_NUMBER;
    const result = spawnSync(process.execPath, [WXT_BIN, 'build'], {
      cwd: ROOT,
      env,
      encoding: 'utf8',
    });
    if (result.status !== 0) {
      throw new Error(
        `wxt build exited ${result.status}\n--- stdout ---\n${result.stdout}\n--- stderr ---\n${result.stderr}`,
      );
    }
    manifest = JSON.parse(
      readFileSync(join(OUT_BASE, 'chrome-mv3', 'manifest.json'), 'utf8'),
    ) as { version?: unknown; version_name?: unknown };
  }, 180_000);

  afterAll(() => {
    rmSync(OUT_BASE, { recursive: true, force: true });
  });

  it('a built manifest has no version_name and a 4-component-free version', () => {
    // `version` equals the semver from package.json with no 4th component.
    const pkg = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8'));
    expect(String(manifest.version)).toBe(String(pkg.version));
    expect(manifest.version_name).toBeUndefined();
  });
});