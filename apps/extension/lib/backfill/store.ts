/**
 * 欠账集合的持久化端口。
 *
 * 不新增任何权限：复用 lib/badge.ts 已经在用的 browser.storage.local（cs_* 前缀同族）。
 *
 * ⚠️ 已知的既有缺口（不是本任务引入的，但会直接打到这条腿）：
 *   wxt.config.ts:9 的 permissions 只有 ['downloads']，**没有 'storage'**，
 *   而 lib/badge.ts:43 已经在用 browser.storage.local。badge 是装饰性的，拿不到
 *   存储就静默 no-op 没关系；**回溯腿不行** —— 没有持久化就没有可断可续，
 *   一路 no-op 会变成「每次重启都从头爬」，正好是我们最怕的那种静默失败。
 *   所以这里拿不到存储时返回 null，engine 会 halt('storage-unavailable') 并留痕，
 *   而不是假装在跑。是否补 'storage' 权限要产品主人拍板（会改 manifest ⇒ 本任务不动）。
 */

export interface BackfillStore {
  load(key: string): Promise<unknown>;
  save(key: string, value: unknown): Promise<void>;
}

type LocalArea = {
  get: (defaults: Record<string, unknown>) => Promise<Record<string, unknown>>;
  set: (values: Record<string, unknown>) => Promise<void>;
};

type ExtApi = { runtime?: { id?: string }; storage?: { local?: unknown } };

/**
 * 🔴 C18 实测发现的【第三道生产阻断】（不是本任务引入的，是本任务撞上的）：
 *
 * 这里原来只读 `globalThis.browser`。**Chrome MV3 里根本没有 `browser` 这个全局**
 * —— 只有 `chrome`。所以在 Chrome 构建里 browserLocalStore() 恒为 null，
 * tickBackfill 第一道闸就是 'no-store'，而 Popup 的开关也【存不下去】。
 *
 * 证据：.output/chrome-mv3/background.js 里 WXT 自己的 browser 垫片写的是
 *   `globalThis.browser?.runtime?.id ? globalThis.browser : globalThis.chrome`
 * —— lib/badge.ts 用的是 WXT 注入的那个 `browser`（所以 badge 在 Chrome 上是好的），
 * 唯独本文件绕过了垫片直接读 globalThis，于是只有 Firefox 能用。
 *
 * 这里逐字照抄 WXT 的判定，不引入任何 import（本文件要在 node 测试环境里裸跑，
 * 那里两个全局都不存在 ⇒ 仍然返回 null ⇒ 既有行为不变）。
 */
function extensionApi(): ExtApi | null {
  const g = globalThis as { browser?: ExtApi; chrome?: ExtApi };
  if (g.browser?.runtime?.id) return g.browser;
  if (g.chrome?.runtime?.id) return g.chrome;
  // runtime.id 都拿不到时退回「谁存在用谁」，免得比原来更严。
  return g.browser ?? g.chrome ?? null;
}

function localArea(): LocalArea | null {
  const area = extensionApi()?.storage?.local as LocalArea | undefined;
  if (!area || typeof area.get !== 'function' || typeof area.set !== 'function') return null;
  return area;
}

/** 拿不到 storage.local 就返回 null —— 让调用方必须显式处理「不能持久化」。 */
export function browserLocalStore(): BackfillStore | null {
  const area = localArea();
  if (!area) return null;
  return {
    async load(key: string): Promise<unknown> {
      const got = await area.get({ [key]: null });
      return got[key] ?? null;
    },
    async save(key: string, value: unknown): Promise<void> {
      await area.set({ [key]: value });
    },
  };
}

/**
 * 读一份 storage.local 的全量快照。
 * 只有 Popup 用得到：它不知道用户当前是哪个平台/哪个账号（那些信息只在实时腿
 * 的那条消息里现成带着），只能把已经存在的欠账集合列出来挑一个显示。
 * 拿不到存储就返回 null —— 和 browserLocalStore() 一样，让调用方显式处理。
 */
export async function browserLocalSnapshot(): Promise<Record<string, unknown> | null> {
  const area = localArea();
  if (!area) return null;
  // 真实 API 里 get(null) = 全量；假实现如果不支持就会抛，交给调用方兜。
  return await (area.get as unknown as (q: null) => Promise<Record<string, unknown>>)(null);
}

/** 纯内存实现，只给测试用。 */
export function memoryStore(seed: Record<string, unknown> = {}): BackfillStore & {
  readonly data: Record<string, unknown>;
  writes: number;
} {
  const data: Record<string, unknown> = { ...seed };
  return {
    data,
    writes: 0,
    async load(key: string): Promise<unknown> {
      // 深拷贝：模拟真实存储的「读回来的是另一个对象」，避免测试里意外共享引用。
      const v = data[key];
      return v === undefined ? null : JSON.parse(JSON.stringify(v));
    },
    async save(key: string, value: unknown): Promise<void> {
      data[key] = JSON.parse(JSON.stringify(value));
      this.writes += 1;
    },
  };
}
