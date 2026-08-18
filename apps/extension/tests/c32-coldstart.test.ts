/**
 * C32-COLDSTART · 冷启动那句「你要做什么才会开始」。
 *
 * 真机上的场景：新用户把开关打开、也打开了一个受支持平台的页面，
 * 于是取数通道是通的（transportWired = true），但回溯目标登记表是空的 ——
 * 登记只发生在「实时腿真的归档过一次对话」的那一脚上。
 * 结果：闹钟每次醒来都无事可做，而**用户不知道自己要做什么**。
 *
 * 🔴 这是【有意的设计】，不是 bug（lib/backfill/alarm.ts:80-87）：
 *    闹钟醒来时 SW 是全新的，没有 tab、没有账号；解法不是去猜一个，
 *    而是只用「用户真的用过的那个账号」。docs/privacy.md:112 —— 扩展没有任何
 *    host 权限，只在用户自己已有的登录态里取数。
 * ⇒ 所以本文件【不碰任何判定逻辑】，只钉死一件事：那句引导语必须存在、
 *   必须说清具体动作与理由，且**在健康机器上不许响**。
 *
 * 全程零网络、零登录态：只有假的 browser.* 和纯函数。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';

const store: Record<string, unknown> = {};

const fakeBrowser: any = {
  runtime: { id: 'mock-extension-id' },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
};

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
});

/**
 * 按真机那条链子算一遍 Popup：开关 → 目标登记表 → tickBlockReason → renderPopup。
 * 🔴 用的是 entrypoints/popup/main.ts 调的那**同一个** tickBlockReason，
 *    不在测试里另写一份闸门判断 —— 另写一份就等于自己给自己打分。
 */
async function popupNow(opts: { enabled: boolean; hasHttp: boolean; hasTargets: boolean }) {
  const { browserLocalStore } = await import('../lib/backfill/store');
  const { setBackfillEnabled, tickBlockReason } = await import('../lib/backfill/schedule');
  const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');

  await setBackfillEnabled(browserLocalStore(), opts.enabled);
  if (opts.hasTargets) {
    store['cs_backfill_targets_v1'] = [
      { platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'acct-fixture-1', at: 1 },
    ];
  }
  const { loadTargets } = await import('../lib/backfill/alarm');
  const targets = await loadTargets(browserLocalStore());

  const block = await tickBlockReason({
    hasStore: true,
    isEnabled: () => opts.enabled,
    isDownloadPaused: () => false,
    hasHttp: opts.hasHttp,
    hasTargets: targets.length > 0,
  });

  const view = renderPopup({
    enabled: opts.enabled,
    block,
    guard: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    lastTick: null,
  });
  return { block, view, text: popupText(view) };
}

/** 引导语必须说清的【具体动作】。措辞可以变，这几件事不许消失。 */
const ACTION_PHRASES = ['发一条消息', '打开一条'];
/** 引导语必须说清的【理由】—— 它把一个像缺陷的限制还原成隐私承诺。 */
const REASON_PHRASES = ['不猜你的账号', 'host 权限'];
/** 🔴 绝不许出现的时间承诺：我们没有速率模型，编一个就是骗人。 */
const FORBIDDEN_PROMISES = ['分钟内', '几分钟', '很快', '马上', '立刻就会开始', '预计'];

describe('C32-COLDSTART · 有通道、没目标时必须把话说全', () => {
  it('🔴 反证：transport 通着、一个目标都没有 ⇒ 必须说清用户要做的那一件事和为什么', async () => {
    const { block, view, text } = await popupNow({
      enabled: true, hasHttp: true, hasTargets: false,
    });
    console.log('[C32-EVIDENCE 冷启动 · 有通道没目标]\n' + text);

    // 前提没被改动：这仍然是 'no-targets'，闸门判定一个字都没动。
    expect(block).toBe('no-targets');
    expect(view.running).toContain('未在运行');
    expect(text).not.toContain('正在归档');

    // 1 · 具体动作。「请稍候」这种废话不算数。
    expect(view.missing).not.toBeNull();
    for (const phrase of ACTION_PHRASES) expect(view.missing!).toContain(phrase);

    // 2 · 为什么要这样 —— 隐私承诺，不是「系统限制」。
    for (const phrase of REASON_PHRASES) expect(view.missing!).toContain(phrase);

    // 3 · 不许承诺我们做不到的事。
    for (const promise of FORBIDDEN_PROMISES) expect(text).not.toContain(promise);
    expect(text).not.toContain('%');
  });

  it('🟢 健康路径守卫：目标确实存在时，那句引导语【一个字都不许出现】', async () => {
    const { block, view, text } = await popupNow({
      enabled: true, hasHttp: true, hasTargets: true,
    });
    console.log('[C32-EVIDENCE 健康机器 · 有目标有通道]\n' + text);

    expect(block).toBeNull();
    expect(view.missing).toBeNull();
    // 🔴 本仓的规矩：新加的提示要在健康机器上验一次「它不响」。
    // 否则它会变成永远挂着的噪声，用户下次真需要看它的时候已经不看了。
    for (const phrase of [...ACTION_PHRASES, ...REASON_PHRASES]) {
      expect(text).not.toContain(phrase);
    }
  });

  it('🟢 守卫二：卡在别的闸门时也不许说这句 —— 那会是**错的**建议', async () => {
    // 开关没打开：此时该做的是打开开关，不是去发消息。
    const off = await popupNow({ enabled: false, hasHttp: true, hasTargets: false });
    expect(off.block).toBe('disabled');
    for (const phrase of [...ACTION_PHRASES, ...REASON_PHRASES]) {
      expect(off.text).not.toContain(phrase);
    }

    // 有目标但没通道：此时该做的是把平台页面开着，不是去发消息。
    const noPort = await popupNow({ enabled: true, hasHttp: false, hasTargets: true });
    expect(noPort.block).toBe('no-http-port');
    for (const phrase of REASON_PHRASES) expect(noPort.text).not.toContain(phrase);
  });
});
