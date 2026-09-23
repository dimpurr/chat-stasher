/**
 * W59b-2 · **`getManifest().version` is not a build identity, and this is the identity
 * that is.**
 *
 * W59 stamped every permanent halt with `runtime.getManifest().version` and made "does
 * this record still apply" mean "does it name this build". That is a sound rule with
 * an unsound input: the version is `<semver>.<n>` **only when `CS_BUILD_NUMBER` is
 * set**, and nothing sets it except `scripts/dev/reload-extension.sh`
 * (lib/build-version.ts). Chrome's Reload button, `wxt dev` and a plain `pnpm build`
 * all produce `0.1.0`, so two builds of different source compared equal, a halt written
 * by the first was read by the second as its own judgement, and the one retry W59
 * exists to give silently never fired. An identity every build shares is not one.
 *
 * The fix is a build stamp (`buildStamp`, `vite.define` in wxt.config.ts, and the same
 * key in vitest.config.ts) composed with the version by `composeBuildId`.
 *
 * ## What two loads still share it — asserted here rather than only written down
 *
 * The same **built bundle**. Re-opening the browser, and Chrome's Reload button on an
 * unchanged `.output/chrome-mv3`, do not re-run the build: the bytes on disk are the
 * same bytes, so it is the same build and reading it as one is correct. A `wxt dev`
 * session that recompiles on a file change is the one place a new bundle can appear
 * without the stamp moving, because the stamp is folded from the config that session
 * started with. Two *invoices* of the build — `pnpm build`, `wxt build`, the reload
 * script — never share it, and that is the property the case below pins.
 */

import { describe, it, expect } from 'vitest';

import { buildStamp } from '../lib/build-version';
import { bakedBuildStamp, composeBuildId, runningBuildId } from '../lib/extension-build';
import { TEST_BUILD_ID, TEST_BUILD_STAMP, TEST_MANIFEST_VERSION, runtimeApi } from './i18n-harness';

describe('W59b-2 · the identity a halt record is stamped with', () => {
  it('🔴 two builds that report the same manifest version are not the same build', () => {
    // The defect in one assertion: `0.1.0` is what every dev-loaded build reports, and
    // on ecbcf2c the two halves of this line were the same string.
    const first = composeBuildId('0.1.0', 'bmtq3x1f');
    const second = composeBuildId('0.1.0', 'bmtq3x1g');
    expect(first).not.toBe(second);
    expect(first).toBe('0.1.0+bmtq3x1f');
    expect(second).toBe('0.1.0+bmtq3x1g');
  });

  it('🔴 a build with no stamp composes to the version exactly, so pre-W59 records read as before', () => {
    expect(composeBuildId('0.1.0.7', null)).toBe('0.1.0.7');
    expect(composeBuildId('0.1.0', null)).toBe('0.1.0');
  });

  it('🔴 the two halves stay readable apart: the stamp cannot be mistaken for a version', () => {
    // A stamp that looked like a semver would make `0.1.0+b1` ambiguous with the
    // `<semver>.<n>` form the reload script produces.
    for (const now of [0, 1_700_000_000_000, Date.parse('2026-09-23T09:00:00.000Z')]) {
      expect(buildStamp(now)).toMatch(/^b[0-9a-z]+$/);
    }
    // Distinct milliseconds are distinct builds — the property the whole fix rests on.
    expect(buildStamp(1_700_000_000_000)).not.toBe(buildStamp(1_700_000_000_001));
  });

  it('🔴 the running build id is the composition, not the bare manifest version', () => {
    // The wiring, asserted against the harness's fake extension API rather than
    // against constants: `runningBuildId()` reads a manifest and a stamp and returns
    // their composition. On ecbcf2c this line answered `0.1.0.1`, the version alone.
    expect(runtimeApi().getManifest().version).toBe(TEST_MANIFEST_VERSION);
    expect(bakedBuildStamp(), 'the suite defines the same key a real build does').toBe(TEST_BUILD_STAMP);
    expect(runningBuildId()).toBe(TEST_BUILD_ID);
    expect(runningBuildId(), 'the version alone must not be the identity').not.toBe(TEST_MANIFEST_VERSION);
  });

  it('🔴 an extension API with no manifest is still "cannot name itself", never a fabricated id', () => {
    // The conservative direction, kept: a build that cannot name itself clears no
    // record (lib/backfill/types.ts's `HaltJudgement`). The composition must not turn
    // that into a string, so it is asserted here next to the identity it belongs to.
    const saved = (globalThis as { browser?: unknown }).browser;
    try {
      (globalThis as { browser?: unknown }).browser = { runtime: { id: 'mock' } };
      expect(runningBuildId()).toBeNull();
    } finally {
      (globalThis as { browser?: unknown }).browser = saved;
    }
  });
});
