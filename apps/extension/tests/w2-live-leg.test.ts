/**
 * W2 · 实时腿的四种结局，一个都不许混。
 *
 *   delivered —— 收到了匹配的 ack ⇒ saved:true
 *   queued    —— 已写进发件箱、还没被确认 ⇒ saved:false（可区分，角标据此计数）
 *   rejected  —— 主机明确 nack 且不可重试 ⇒ 保留、可见、不再自动重试
 *   refused   —— 我们连收都没收下（发件箱满 / 读不出来 / 起不出名字）⇒ 具名原因
 *
 * 🔴 这个文件里最重要的一条是 write-ahead：
 *    投递【发生的那一刻】，payload 必须已经在发件箱里躺着。
 *    断言方式是：桩在收到投递请求时去读发件箱，必须读得到这一条。
 *
 * 全程走【真实的 background 入口】（runtime.onMessage('chat-captured')），
 * 只把 browser.* 与主机换成可编程的桩。
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];

/** 主机每一刻的行为。 */
let hostMode: 'up' | 'down' | 'nack-nonretryable' | 'nack-retryable';
/** 投递请求到达时，发件箱里有没有这一条（write-ahead 的直接证据）。 */
let outboxHadPayloadAtDelivery: boolean | null = null;
let deliveries: Array<{ name: string; payload: string }> = [];
let lastCapturedAt = 1_700_000_000_000;

async function sha256Of(text: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

const fakeBrowser: any = {
  runtime: {
    id: 'w2-live-leg',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    async sendNativeMessage(host: string, message: Record<string, any>) {
      if (message.type === 'deliver') {
        // 🔴 write-ahead 的检查点：投递【就在此刻】发生 —— 主机是死是活都一样。
        const { getEntry } = await import('../lib/outbox');
        const lookup = await getEntry(message.sha256);
        outboxHadPayloadAtDelivery = lookup.ok && lookup.entry !== null;
      }
      if (hostMode === 'down') throw new Error('Specified native messaging host not found.');
      if (message.type === 'hello') {
        return { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: '/stage' };
      }
      if (message.type === 'deliver') {
        if (hostMode === 'nack-nonretryable' || hostMode === 'nack-retryable') {
          const retryable = hostMode === 'nack-retryable';
          return {
            protocol: 1, type: 'nack', request_id: message.request_id,
            kind: retryable ? 'integrity' : 'invalid-bundle', retryable,
            detail: retryable ? 'sha256 does not match payload' : 'payload is not an inbox bundle',
          };
        }
        deliveries.push({ name: message.name, payload: message.payload });
        return {
          protocol: 1, type: 'ack', request_id: message.request_id,
          status: 'stored', sha256: message.sha256, shard: `0001-${message.name}`,
        };
      }
      throw new Error(`unexpected ${message.type}`);
    },
  },
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
  action: {
    badgeText: '' as string,
    async setBadgeText(o: { text: string }) { fakeBrowser.action.badgeText = o.text; },
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

const SID = 'aaaaaaaa-1111-2222-3333-444444444444';

function capture(overrides: Partial<CapturedFetch> = {}): CapturedFetch {
  return {
    url: `https://chatgpt.com/backend-api/conversation/${SID}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-1' }),
    pageUrl: `https://chatgpt.com/c/${SID}`,
    capturedAt: (lastCapturedAt += 1),
    ...overrides,
  };
}

async function dispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  const replied = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return replied;
}

/**
 * 把发件箱的「已用字节」写到只差 1 字节就满。
 *
 * 用的是模块自己导出的库名/仓库名/计数器键 —— 不是另一份实现，只是把那个
 * 计数器直接推到上限，好让真实的判断分支（`used + bytes > capacity`）真的走到。
 * 真写 256 MiB 也可以，但那只是更慢更脆的同一条断言。
 */
async function fillCounterToCapacity(): Promise<void> {
  const { OUTBOX_DB_NAME, OUTBOX_DB_VERSION, OUTBOX_META_STORE, OUTBOX_META_BYTES_KEY, OUTBOX_CAPACITY_BYTES } =
    await import('../lib/outbox');
  const db: IDBDatabase = await new Promise((resolve, reject) => {
    const req = (globalThis as any).indexedDB.open(OUTBOX_DB_NAME, OUTBOX_DB_VERSION);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(OUTBOX_META_STORE, 'readwrite');
    tx.objectStore(OUTBOX_META_STORE).put(OUTBOX_CAPACITY_BYTES - 1, OUTBOX_META_BYTES_KEY);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
  db.close();
}

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
  hostMode = 'up';
  outboxHadPayloadAtDelivery = null;
  deliveries = [];
  lastCapturedAt = 1_700_000_000_000;
  fakeBrowser.action.badgeText = '';
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

describe('W2-LIVE · delivered', () => {
  it('🔴 匹配的 ack ⇒ saved:true、status:delivered；而且【投递时 payload 已经在发件箱里】', async () => {
    const result = await dispatch(capture());

    expect(result.ok).toBe(true);
    expect(result.saved).toBe(true);
    expect(result.status).toBe('delivered');
    expect(result.channel).toBe('native-messaging');
    expect(result.finalName).toBe(`chatgpt-${SID}.json`);
    expect(deliveries).toHaveLength(1);

    // 🔴 write-ahead：桩在投递那一刻读发件箱，条目必须已经在那儿。
    expect(outboxHadPayloadAtDelivery).toBe(true);

    // ack 之后条目被删掉，角标回到空。
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    expect(fakeBrowser.action.badgeText).toBe('');
  });
});

describe('W2-LIVE · queued（已入队、未确认）', () => {
  it('🔴 主机不在 ⇒ saved:false、status:queued（不是成功，也不是失败），条目留在发件箱，角标显示 1', async () => {
    hostMode = 'down';
    const result = await dispatch(capture());

    expect(result.ok).toBe(false);          // ok === saved，两者都不许说谎
    expect(result.saved).toBe(false);
    expect(result.status).toBe('queued');
    expect(result.reason).toBe('send-failed');
    expect(outboxHadPayloadAtDelivery).toBe(true);   // 同样先落盘再尝试

    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ name: `chatgpt-${SID}.json`, state: 'pending', attempts: 1 });
    expect(fakeBrowser.action.badgeText).toBe('1');
  });

  it('主机回来之后，下一次心跳就把这条送出去并销账（角标清空）', async () => {
    hostMode = 'down';
    await dispatch(capture());
    const { listEntries } = await import('../lib/outbox');
    expect((await listEntries())!).toHaveLength(1);

    hostMode = 'up';
    const { drainOutbox } = await import('../lib/outbox');
    // 下一次心跳：退避窗口早就过去了（真实的闹钟是 5 分钟一跳）。
    const report = await drainOutbox({ now: () => Date.now() + 10 * 60_000 });
    expect(report).toMatchObject({ delivered: 1, stoppedBy: 'drained' });
    expect(await listEntries()).toEqual([]);
    expect(deliveries).toHaveLength(1);
  });
});

describe('W2-LIVE · rejected（主机明确说这条不行）', () => {
  it('🔴 非 retryable nack ⇒ saved:false、status:rejected、带上 kind，条目【保留】', async () => {
    hostMode = 'nack-nonretryable';
    const result = await dispatch(capture());

    expect(result.saved).toBe(false);
    expect(result.status).toBe('rejected');
    expect(result.kind).toBe('invalid-bundle');
    expect(result.reason).toContain('invalid-bundle');

    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ state: 'rejected', rejectKind: 'invalid-bundle' });
    expect(fakeBrowser.action.badgeText).toBe('!');   // 告警态，不是数字
  });

  it('retryable nack ⇒ 仍然只是 queued（主机说"再试试"，不是"这条不行"）', async () => {
    hostMode = 'nack-retryable';
    const result = await dispatch(capture());
    expect(result).toMatchObject({ saved: false, status: 'queued' });
    // 结局是「还没送达」，理由是主机给的那个（kind 就在里面），不是被判死。
    expect(result.reason).toContain('nack');
    expect(result.reason).toContain('integrity');
    expect(result.kind).toBeUndefined();
    const entries = (await (await import('../lib/outbox')).listEntries())!;
    expect(entries[0]!.state).toBe('pending');
  });
});

describe('W2-LIVE · refused（我们连收都没收下）', () => {
  it('🔴 发件箱满了 ⇒ 这次抓取被拒、原因可见；已排队的一条不少、一个字节不改', async () => {
    const ob = await import('../lib/outbox');
    // 先真的放一条进去（这样"旧的没被动过"才有东西可指）。主机先下线 ⇒ 它留在队里。
    hostMode = 'down';
    const firstResult = await dispatch(capture());
    expect(firstResult).toMatchObject({ saved: false, status: 'queued' });
    const before = await ob.listEntries();
    expect(before).toHaveLength(1);
    hostMode = 'up';

    // 把「已用字节」直接推到上限：meta 里的那个计数器就是唯一的帐。
    // （比真写 256 MiB 更快、更稳，也同样是走真实的判断分支。）
    await fillCounterToCapacity();

    const result = await dispatch(capture());
    expect(result.saved).toBe(false);
    expect(result.status).toBe('refused');
    expect(result.reason).toBe('outbox-full');

    const after = await ob.listEntries();
    expect(after).toEqual(before);   // 🔴 一条不少、一个字节不改
    console.log('[W2-LIVE] 满了之后拒收的返回:', result);
  });

  it('🔴 起不出会话身份 ⇒ refused，一个字节都不进发件箱', async () => {
    const result = await dispatch(capture({
      url: 'https://chatgpt.com/backend-api/conversation/shortid',
      pageUrl: undefined,
    }));
    expect(result).toMatchObject({ saved: false, status: 'refused' });
    expect(result.reason).toContain('no-session-id');
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    expect(deliveries).toEqual([]);
  });

  it('🔴 IndexedDB 读不出来 ⇒ refused/queued 且写明 outbox-unavailable，绝不冒充成功', async () => {
    delete (globalThis as any).indexedDB;
    vi.resetModules();
    const result = await dispatch(capture());
    expect(result.saved).toBe(false);
    expect(result.reason).toBe('outbox-unavailable');
    expect(deliveries).toEqual([]);            // 没写进发件箱就不投递
  });
});
