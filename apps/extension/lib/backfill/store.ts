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

function localArea(): LocalArea | null {
  const area = (globalThis as { browser?: { storage?: { local?: unknown } } }).browser?.storage
    ?.local as LocalArea | undefined;
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
