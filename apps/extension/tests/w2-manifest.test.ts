/**
 * W2 · 权限表：`downloads` 必须消失，`unlimitedStorage` 必须出现。
 *
 * 这条用例为什么值得存在：权限表是**声明式**的。删掉 `lib/download.ts` 之后，
 * 如果 manifest 还留着 `downloads`，浏览器会继续弹「管理你的下载内容」，
 * 而代码里已经一个字都用不到它 —— 用户为一项不存在的功能付了代价，
 * 而没有任何测试会红。
 *
 * 🔴 断言分三层，缺一层都不够：
 *   1. `wxt.config.ts` 里的 permissions 数组本身（构建的输入）；
 *   2. 源码里再也找不到 downloads API 的调用点（构建的原料）；
 *   3. 构建产物 `.output/chrome-mv3/manifest.json` 如果存在，必须与 1 一致
 *      （构建的输出）—— 收口时 `pnpm -s build` 之后它就存在，那时这一层是真的在跑。
 */

import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = new URL('..', import.meta.url).pathname;

function read(rel: string): string {
  return readFileSync(join(ROOT, rel), 'utf8');
}

/** wxt.config.ts 里的 permissions 数组，逐字读出来（不 import：那是构建期的配置）。 */
function configuredPermissions(): string[] {
  const src = read('wxt.config.ts');
  const match = /permissions:\s*\[([^\]]*)\]/.exec(src);
  if (!match) throw new Error('wxt.config.ts has no permissions array');
  return [...match[1]!.matchAll(/'([^']+)'/g)].map((m) => m[1]!);
}

/** 源码文件清单（不带 .wxt / node_modules / tests）。 */
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
  it('🔴 不含 downloads', () => {
    const permissions = configuredPermissions();
    console.log('[W2-MANIFEST] 配置里的 permissions:', permissions);
    expect(permissions).not.toContain('downloads');
  });

  it('🔴 含 unlimitedStorage / nativeMessaging，并保留 storage 与 alarms', () => {
    const permissions = configuredPermissions();
    expect(permissions).toContain('unlimitedStorage');
    expect(permissions).toContain('nativeMessaging');
    expect(permissions).toContain('storage');
    expect(permissions).toContain('alarms');
    // 闭集：多出来的每一项都要有理由（理由写在 wxt.config.ts 的注释里）。
    expect([...permissions].sort()).toEqual(
      ['alarms', 'nativeMessaging', 'storage', 'unlimitedStorage'].sort(),
    );
  });

  it('🔴 注释里逐项解释了这几个权限（省得以后有人凭感觉加一个）', () => {
    const src = read('wxt.config.ts');
    for (const why of ['unlimitedStorage', 'storage', 'alarms', 'nativeMessaging']) {
      expect([why, new RegExp(`Why '${why}'`).test(src)]).toEqual([why, true]);
    }
  });

  it('🔴 源码里再也没有任何 downloads 调用点', () => {
    const offenders: string[] = [];
    for (const file of sourceFiles()) {
      const text = readFileSync(file, 'utf8');
      if (/browser\.downloads|chrome\.downloads|downloads\.download\(|downloads\.removeFile|downloads\.onChanged/.test(text)) {
        offenders.push(file.replace(ROOT, ''));
      }
    }
    console.log('[W2-MANIFEST] 引用 downloads API 的文件:', offenders);
    expect(offenders).toEqual([]);
  });

  it('🔴 被删掉的两个模块确实不在了', () => {
    expect(existsSync(join(ROOT, 'lib/download.ts'))).toBe(false);
    expect(existsSync(join(ROOT, 'lib/download-guard.ts'))).toBe(false);
  });

  it('🔴 传输层用的是 sendNativeMessage，不是 connectNative（§2）', () => {
    const src = read('lib/native-host.ts');
    expect(src).toContain('sendNativeMessage');
    // 注释里提到 connectNative 是可以的（那里正是在说"不用它"）；
    // 这里禁的是**调用**。
    expect(src).not.toMatch(/\.connectNative\s*\(/);
    for (const file of sourceFiles()) {
      const text = readFileSync(file, 'utf8');
      expect([file.replace(ROOT, ''), /\.connectNative\s*\(/.test(text)]).toEqual([
        file.replace(ROOT, ''), false,
      ]);
    }
  });

  it('构建产物存在时，它的 permissions 必须与配置一致', () => {
    const built = join(ROOT, '.output/chrome-mv3/manifest.json');
    if (!existsSync(built)) {
      console.log('[W2-MANIFEST] .output 还不存在（测试跑在 build 之前）—— 这一层本次未验；'
        + '收口时用 `pnpm -s build` 的产物人工核对。');
      return;
    }
    const manifest = JSON.parse(readFileSync(built, 'utf8')) as { permissions?: string[] };
    console.log('[W2-MANIFEST] 构建产物的 permissions:', manifest.permissions);
    expect(manifest.permissions).not.toContain('downloads');
    expect(manifest.permissions).toContain('unlimitedStorage');
    expect(manifest.permissions).toContain('nativeMessaging');
    expect([...(manifest.permissions ?? [])].sort()).toEqual([...configuredPermissions()].sort());
  });
});
