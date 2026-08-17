/**
 * C19 · 让回溯腿【自己醒】。
 *
 * ## 为什么必须加闹钟
 * C13 选的心跳是「实时腿捕到一条时顺带踢一脚」。那个选择在当时是对的
 * （零成本、目标信息现成、时机温和），但它有一个致命推论：
 * 🔴 **一个装了扩展之后再也不打开那个网站的用户，历史永远补不完** ——
 *    因为再也不会有第二条实时捕获来踢它。
 * 回溯腿的产品承诺是「好几天之后慢慢全部补上」，靠一个只在用户主动聊天时
 * 才响一下的心跳是兑现不了的。
 *
 * ## 代价（已查证，2026-08-17）
 * Chrome 的权限警告清单里，`alarms` 与 `storage` 一样【不弹任何安装警告】；
 * 会弹的是我们已经有的 `downloads`（「管理你的下载内容」）。
 * ⇒ 加这一个权限对用户是无感的，而它换来的是「这条腿真的会自己往前走」。
 *
 * ## 🔴 默认仍然是关的
 * BACKFILL_DEFAULT_ENABLED 一个字都没改（仍然是 false）。
 * **闹钟只在开关打开时才创建，关掉时立刻清掉** —— 没同意就没有闹钟，
 * 也就没有任何周期性行为。
 */

import type { BackfillStore } from './store';

/** 闹钟名。同一个名字重复 create 会覆盖，天然幂等。 */
export const BACKFILL_ALARM_NAME = 'cs-backfill-tick';

/**
 * 🔴 周期 = 5 分钟。这个数是算出来的，不是拍的：
 *
 *  · **下限**：Chrome 对 MV3 打包扩展的 alarm 周期有最小值（1 分钟）；比它小的
 *    值会被浏览器悄悄抬上去，写一个抬不上去的数只会让代码和实际行为对不上。
 *  · **上限由日上限决定**：一次 tick 只清 1 笔账（DEFAULT_TICK_DETAILS = 1），
 *    而每天的配额是 DEFAULT_DETAIL_PACE.maxPerDay = 200 条。
 *    5 分钟一次 ⇒ 一天最多醒 288 次 > 200 ⇒ **真正卡住速度的是「每天上限」，
 *    不是闹钟**。这正是产品主人定的方向：温和度以每天上限为主。
 *    （若周期取 10 分钟，一天只有 144 次 < 200，闹钟反而成了瓶颈，
 *      1000 条要拖到 7 天以上，且日上限形同虚设。）
 *  · **与每条间隔不打架**：300 秒 ≫ 每条 20 秒的最小间隔 ⇒ 闹钟路径上
 *    间隔闸门永远是 0 等待；间隔真正起作用的是实时腿连着踢的时候（C19 任务 3）。
 *  · 每次醒来只做一件很小的事（读 storage、最多取 1 条），对 MV3 的 SW 生命周期
 *    友好 —— 短 tick × 很多次，与长 tick 在进度上等价，但更温和。
 */
export const BACKFILL_ALARM_PERIOD_MINUTES = 5;

export interface AlarmsApi {
  create(name: string, info: { periodInMinutes?: number; delayInMinutes?: number }): void | Promise<void>;
  clear(name: string): boolean | Promise<boolean>;
  get?(name: string): Promise<unknown>;
}

export type AlarmSyncResult = 'created' | 'kept' | 'cleared' | 'unavailable';

/**
 * 让闹钟与开关保持一致。**这是闹钟生命周期的唯一入口。**
 *  · 开关开 ⇒ 有闹钟（已经有的就不动，免得每次 SW 醒来都把周期从头计时）；
 *  · 开关关 ⇒ 清掉。
 * 拿不到 alarms API（比如 node 测试环境）返回 'unavailable'，绝不假装成功。
 */
export async function syncBackfillAlarm(
  alarms: AlarmsApi | null | undefined,
  enabled: boolean,
): Promise<AlarmSyncResult> {
  if (!alarms || typeof alarms.create !== 'function' || typeof alarms.clear !== 'function') {
    return 'unavailable';
  }
  if (!enabled) {
    await alarms.clear(BACKFILL_ALARM_NAME);
    return 'cleared';
  }
  if (typeof alarms.get === 'function') {
    const existing = await alarms.get(BACKFILL_ALARM_NAME);
    if (existing) return 'kept';
  }
  await alarms.create(BACKFILL_ALARM_NAME, { periodInMinutes: BACKFILL_ALARM_PERIOD_MINUTES });
  return 'created';
}

// ---------------------------------------------------------------------------
// 回溯目标登记表
//
// 闹钟醒来时 SW 是全新的：没有当前 tab、没有账号、什么都不知道 ——
// 这正是 C13 当初拒绝用定时器的理由之一。解法不是去猜，而是把实时腿【已经
// 现成带着】的那份目标（platform / origin / scope）在踢那一脚时顺手记下来。
// 于是闹钟用的永远是"用户真的用过的那个账号"，一个字都不用编。
// ---------------------------------------------------------------------------

export const BACKFILL_TARGETS_KEY = 'cs_backfill_targets_v1';
export const MAX_TARGET_ENTRIES = 8;

export interface BackfillTarget {
  platform: string;
  origin: string;
  scope: string;
  /** 最近一次见到这个目标的时刻。只用来排序。 */
  at: number;
}

function isTarget(v: unknown): v is BackfillTarget {
  if (!v || typeof v !== 'object') return false;
  const t = v as Partial<BackfillTarget>;
  return typeof t.platform === 'string'
    && typeof t.origin === 'string'
    && typeof t.scope === 'string'
    && typeof t.at === 'number';
}

export async function loadTargets(store: BackfillStore | null): Promise<BackfillTarget[]> {
  if (!store) return [];
  const raw = await store.load(BACKFILL_TARGETS_KEY);
  return Array.isArray(raw) ? raw.filter(isTarget) : [];
}

/** 记一个目标（platform+scope 去重，最近的排最前）。 */
export async function rememberTarget(
  store: BackfillStore | null,
  target: BackfillTarget,
): Promise<BackfillTarget[]> {
  if (!store) return [];
  const rest = (await loadTargets(store)).filter(
    (t) => !(t.platform === target.platform && t.scope === target.scope),
  );
  const next = [target, ...rest].slice(0, MAX_TARGET_ENTRIES);
  await store.save(BACKFILL_TARGETS_KEY, next);
  return next;
}
