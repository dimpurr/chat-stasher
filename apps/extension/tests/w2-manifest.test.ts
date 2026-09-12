/**
 * W2 · The permission list: `downloads` must be gone and `unlimitedStorage` must be there.
 *
 * Why this case is worth having: the permission list is **declarative**. After lib/download.ts
 * was deleted, a leftover `downloads` in the manifest would keep showing "Manage your
 * downloads" while not one line of code can use it — the user paying for a feature that no
 * longer exists, with no test going red.
 *
 * 🔴 The assertions come in three layers, and missing one is not enough:
 *   1. the permissions array in `wxt.config.ts` itself (the build's input);
 *   2. no call site of the downloads API exists anywhere in the source (the build's material);
 *   3. if the build output `.output/chrome-mv3/manifest.json` exists, it must agree with 1
 *      (the build's output) — it exists after `pnpm -s build` at closing time, which is when
 *      this layer really runs.
 */

import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = new URL('..', import.meta.url).pathname;

function read(rel: string): string {
  return readFileSync(join(ROOT, rel), 'utf8');
}

/** The permissions array in wxt.config.ts, read out byte for byte (not imported: that is build-time config). */
function configuredPermissions(): string[] {
  const src = read('wxt.config.ts');
  const match = /permissions:\s*\[([^\]]*)\]/.exec(src);
  if (!match) throw new Error('wxt.config.ts has no permissions array');
  return [...match[1]!.matchAll(/'([^']+)'/g)].map((m) => m[1]!);
}

/** The source file list (without .wxt / node_modules / tests). */
function sourceFiles(dir = ROOT, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    if (['node_modules', '.wxt', '.output', 'tests', '.git'].includes(name)) continue;
    const abs = join(dir, name);
    if (statSync(abs).isDirectory()) sourceFiles(abs, out);
    else if (/\.(ts|html)$/.test(name)) out.push(abs);
  }
  return out;
}

describe('W2-MANIFEST · permissions', () => {
  it('🔴 does not contain downloads', () => {
    const permissions = configuredPermissions();
    console.log('[W2-MANIFEST] permissions in the config:', permissions);
    expect(permissions).not.toContain('downloads');
  });

  it('🔴 contains unlimitedStorage / nativeMessaging, and keeps storage and alarms', () => {
    const permissions = configuredPermissions();
    expect(permissions).toContain('unlimitedStorage');
    expect(permissions).toContain('nativeMessaging');
    expect(permissions).toContain('storage');
    expect(permissions).toContain('alarms');
    // A closed set: every extra item needs a reason (the reasons are in wxt.config.ts's comments).
    expect([...permissions].sort()).toEqual(
      ['alarms', 'nativeMessaging', 'storage', 'unlimitedStorage'].sort(),
    );
  });

  it('🔴 the comments explain each of these permissions (so nobody adds one on a hunch later)', () => {
    const src = read('wxt.config.ts');
    for (const why of ['unlimitedStorage', 'storage', 'alarms', 'nativeMessaging']) {
      expect([why, new RegExp(`Why '${why}'`).test(src)]).toEqual([why, true]);
    }
  });

  it('🔴 there is no downloads call site left anywhere in the source', () => {
    const offenders: string[] = [];
    for (const file of sourceFiles()) {
      const text = readFileSync(file, 'utf8');
      if (/browser\.downloads|chrome\.downloads|downloads\.download\(|downloads\.removeFile|downloads\.onChanged/.test(text)) {
        offenders.push(file.replace(ROOT, ''));
      }
    }
    console.log('[W2-MANIFEST] files referencing the downloads API:', offenders);
    expect(offenders).toEqual([]);
  });

  it('🔴 the two deleted modules really are gone', () => {
    expect(existsSync(join(ROOT, 'lib/download.ts'))).toBe(false);
    expect(existsSync(join(ROOT, 'lib/download-guard.ts'))).toBe(false);
  });

  it('🔴 the transport uses sendNativeMessage, not connectNative (§2)', () => {
    const src = read('lib/native-host.ts');
    expect(src).toContain('sendNativeMessage');
    // Mentioning connectNative in a comment is fine (that is precisely where it says "do not use it");
    // what is forbidden here is **calling** it.
    expect(src).not.toMatch(/\.connectNative\s*\(/);
    for (const file of sourceFiles()) {
      const text = readFileSync(file, 'utf8');
      expect([file.replace(ROOT, ''), /\.connectNative\s*\(/.test(text)]).toEqual([
        file.replace(ROOT, ''), false,
      ]);
    }
  });

  it('when the build output exists, its permissions must agree with the config', () => {
    const built = join(ROOT, '.output/chrome-mv3/manifest.json');
    if (!existsSync(built)) {
      console.log('[W2-MANIFEST] .output does not exist yet (tests run before build) — this layer was not exercised;'
        + ' check it by hand against the `pnpm -s build` output at closing time.');
      return;
    }
    const manifest = JSON.parse(readFileSync(built, 'utf8')) as { permissions?: string[] };
    console.log('[W2-MANIFEST] permissions in the build output:', manifest.permissions);
    expect(manifest.permissions).not.toContain('downloads');
    expect(manifest.permissions).toContain('unlimitedStorage');
    expect(manifest.permissions).toContain('nativeMessaging');
    expect([...(manifest.permissions ?? [])].sort()).toEqual([...configuredPermissions()].sort());
  });
});
