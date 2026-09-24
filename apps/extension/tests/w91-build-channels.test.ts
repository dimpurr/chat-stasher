/**
 * W91 · **The build test: the generated manifest must differ by release channel.**
 *
 * The stable/dev split is not only a runtime filter — the owner decision is "least
 * privilege for store review", so a **stable** build must not even declare the
 * experimental platforms' origins in its `content_scripts.matches`, and must take
 * no `host_permissions` at all. A dev build must still declare every origin, or
 * the e2e suite (which runs on the dev channel and covers Perplexity) would have
 * nothing to load.
 *
 * ## Why this runs the real build instead of reading `CONTENT_MATCHES`
 *
 * `tests/w91-channels.test.ts` already asserts the derived sets. What it cannot
 * assert is the step between them and the artifact a store reviewer opens:
 * `wxt.config.ts`'s `define` for `__CS_RELEASE_CHANNEL__`, the WXT content-script
 * manifest generation, and the absence of a second, unconditional match set. A
 * unit test that re-derives the same set twice would pass even if the manifest
 * were built from a different source. So this builds both channels — into separate
 * `CS_OUT_DIR`s so neither clobbers the other — and reads the manifests back.
 *
 * 🔴 The build is given ~2 minutes: a cold WXT build of this extension is a few
 *    seconds, but the suite runs builds under whatever load the machine is under,
 *    and a channel regression is worth waiting for.
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { readFileSync, rmSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';

const ROOT = new URL('..', import.meta.url).pathname; // apps/extension
const WXT_BIN = join(ROOT, 'node_modules', 'wxt', 'bin', 'wxt.mjs');
const OUT_BASE = join(ROOT, '.output', 'w91-channels');

const STABLE_ORIGINS = [
  'https://chat.deepseek.com/*',
  'https://chatgpt.com/*',
  'https://chat.openai.com/*',
  'https://gemini.google.com/*',
  'https://claude.ai/*',
  'https://grok.com/*',
];
const EXPERIMENTAL_ORIGINS = ['https://www.perplexity.ai/*', 'https://www.kimi.com/*'];

interface Manifest {
  content_scripts?: { matches?: string[] }[];
  host_permissions?: string[];
  permissions?: string[];
}

/** Build one channel into its own out dir and return its manifest. */
function buildManifest(channel: 'stable' | 'dev'): Manifest {
  const outDir = join(OUT_BASE, channel);
  const env: NodeJS.ProcessEnv = { ...process.env, CS_OUT_DIR: outDir };
  if (channel === 'dev') env.CS_RELEASE_CHANNEL = 'dev';
  else delete env.CS_RELEASE_CHANNEL;

  const result = spawnSync(process.execPath, [WXT_BIN, 'build'], {
    cwd: ROOT,
    env,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    throw new Error(
      `wxt build (${channel}) exited ${result.status}\n--- stdout ---\n${result.stdout}\n--- stderr ---\n${result.stderr}`,
    );
  }
  return JSON.parse(readFileSync(join(outDir, 'chrome-mv3', 'manifest.json'), 'utf8')) as Manifest;
}

/** Every origin every content script of the manifest matches. */
function matchedOrigins(manifest: Manifest): string[] {
  return [...new Set((manifest.content_scripts ?? []).flatMap((script) => script.matches ?? []))].sort();
}

let stable: Manifest;
let dev: Manifest;

describe('W91-4 · the generated manifest, per channel', () => {
  beforeAll(() => {
    rmSync(OUT_BASE, { recursive: true, force: true });
    stable = buildManifest('stable');
    dev = buildManifest('dev');
  }, 180_000);

  afterAll(() => {
    rmSync(OUT_BASE, { recursive: true, force: true });
  });

  it('🔴 the stable manifest matches no experimental origin, and no <all_urls>', () => {
    const matches = matchedOrigins(stable);
    for (const origin of EXPERIMENTAL_ORIGINS) {
      expect(matches, `stable manifest must not match ${origin}`).not.toContain(origin);
    }
    expect(matches).not.toContain('<all_urls>');
    expect(matches).toEqual([...STABLE_ORIGINS].sort());
  });

  it('🔴 the dev manifest matches every origin, including both experimental ones', () => {
    const matches = matchedOrigins(dev);
    for (const origin of EXPERIMENTAL_ORIGINS) {
      expect(matches, `dev manifest must match ${origin}`).toContain(origin);
    }
    expect(matches).toEqual([...STABLE_ORIGINS, ...EXPERIMENTAL_ORIGINS].sort());
  });

  it('🔴 neither channel declares host_permissions, and the permission set is the closed four', () => {
    for (const [channel, manifest] of [['stable', stable], ['dev', dev]] as const) {
      expect(manifest.host_permissions ?? [], `${channel} host_permissions`).toEqual([]);
      expect([...(manifest.permissions ?? [])].sort(), `${channel} permissions`).toEqual(
        ['alarms', 'nativeMessaging', 'storage', 'unlimitedStorage'].sort(),
      );
    }
  });
});
