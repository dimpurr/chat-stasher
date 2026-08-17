/**
 * C19 · 回溯腿的【取数通道】—— 第二道闸的答案。
 *
 * ## 问题
 * C13 把 http 端口写成"必须有人显式注入"，然后【没有任何生产代码注入它】。
 * 于是回溯腿在浏览器里永远停在 'no-http-port'（C17-4 实测）。
 * 要接上它，就得回答一个真问题：**谁来发这个请求？**
 *
 * ## 为什么不能在 background 里直接 fetch
 * MV3 的 service worker 里 `fetch('https://chatgpt.com/backend-api/...')` 是一个
 * **跨源**请求：SW 自己的源是 chrome-extension://<id>。要让它带上用户在 chatgpt.com
 * 的 cookie，必须声明 `host_permissions: ["https://chatgpt.com/*", ...]` ——
 * 那是【新增权限】，而且是安装时会弹「读取和更改你在 xxx 上的数据」的那一类。
 * 🔴 本任务硬约束：不许新增 host 权限。所以这条路直接封死。
 *
 * ## 走通的那条路：让【已经存在的内容脚本】去取
 * 内容脚本已经按 CONTENT_MATCHES 注入在这些平台的页面里（lib/contract.ts:292）。
 * 在那个上下文里发起的**同源** fetch，走的就是用户自己那个页面的凭据 ——
 * 与用户手动点开一条历史对话时浏览器发出的请求同源、同 cookie、同 UA。
 * 这正好满足「取数必须发生在用户已登录的浏览器上下文里」这个架构前提，
 * 而且【不需要任何新权限】：内容脚本对自己所在页面的同源请求本来就不受 host
 * 权限约束，matches 一个字都不用改。
 *
 * ## 因此这条腿的边界（诚实写在这里，Popup 也照这个说）
 * 🔴 **必须有一个该平台的标签页开着**，回溯才取得到数。没有开着的页面 ⇒
 *    没有可用端口 ⇒ tickBackfill 仍然如实返回 'no-http-port'。
 *    「装了之后再也不打开那个网站」的用户，历史确实补不完 —— 这是架构前提的
 *    直接推论，不是 bug，也不许拿假状态盖过去。
 *
 * ## 三道自我约束（内容脚本侧，见 serveBackfillFetch）
 *  1. 只允许**同源**：请求 URL 的 origin 必须逐字等于页面自己的 origin；
 *  2. 只允许**平台表里的源**；
 *  3. 只允许**回溯腿自己那两条路径**（会话列表 / 会话正文）。
 * 任何一条不满足就拒绝并回错，绝不代发。这样即便消息通道被别的东西碰到，
 * 内容脚本也不会变成一个"任意 URL 代理"。
 */

import { getPlatformByOrigin, MAX_RAW_BYTES } from '../contract';
import { CHATGPT_DETAIL_PATH, CHATGPT_LIST_PATH } from './enumerate';
import type { HttpPort, HttpResponse } from './engine';
import type { BackfillStore } from './store';

/** background → 内容脚本：帮我取这个 URL。 */
export const BACKFILL_FETCH_MESSAGE = 'cs-backfill-fetch';
/** background → 内容脚本：你还活着吗（用来如实回答 Popup 的 transportWired）。 */
export const BACKFILL_PING_MESSAGE = 'cs-backfill-ping';
/** 内容脚本 → background：我在某个平台页面上活着，tab id 由 sender 带来。 */
export const BACKFILL_TAB_HELLO_MESSAGE = 'cs-backfill-tab-hello';

/** 活着的平台标签页登记表。cs_* 前缀同族，不新增权限。 */
export const BACKFILL_TABS_KEY = 'cs_backfill_tabs_v1';
/** 登记表最多留几条。够覆盖"同时开着几个平台"，又不会无限长。 */
export const MAX_TAB_ENTRIES = 12;

export interface TabEntry {
  tabId: number;
  origin: string;
  at: number;
}

export type BackfillFetchReply =
  | { ok: true; status: number; text: string }
  | { ok: false; error: string };

function isRecord(v: unknown): v is Record<string, unknown> {
  return !!v && typeof v === 'object';
}

export function isBackfillFetchRequest(v: unknown): v is { type: string; url: string } {
  return isRecord(v) && v.type === BACKFILL_FETCH_MESSAGE && typeof v.url === 'string';
}

export function isBackfillPing(v: unknown): v is { type: string } {
  return isRecord(v) && v.type === BACKFILL_PING_MESSAGE;
}

export function isTabHello(v: unknown): v is { type: string; origin: string } {
  return isRecord(v) && v.type === BACKFILL_TAB_HELLO_MESSAGE && typeof v.origin === 'string';
}

/**
 * 🔴 内容脚本敢不敢代发这个 URL。三条全过才行（理由见文件头）。
 * 判断是纯函数 ⇒ 可以被单独断言，不依赖浏览器。
 */
export function isAllowedBackfillUrl(url: string, pageOrigin: string): boolean {
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return false;
  }
  if (u.origin !== pageOrigin) return false;            // 1 · 同源
  if (!getPlatformByOrigin(u.origin)) return false;     // 2 · 在平台表里
  return u.pathname === CHATGPT_LIST_PATH               // 3 · 只有这两条路径
    || u.pathname.startsWith(CHATGPT_DETAIL_PATH);
}

export type FetchLike = (url: string) => Promise<{ status: number; text: () => Promise<string> }>;

/**
 * 内容脚本侧的取数实现。**这段代码跑在用户已登录的那个页面的上下文里。**
 * 任何失败都变成 `{ok:false}`，绝不把异常抛回消息通道（抛出去会变成一句
 * 看不懂的 "Could not establish connection"）。
 */
export async function serveBackfillFetch(
  url: string,
  pageOrigin: string,
  fetchImpl: FetchLike,
): Promise<BackfillFetchReply> {
  if (!isAllowedBackfillUrl(url, pageOrigin)) {
    return { ok: false, error: 'refused: url is not a same-origin backfill endpoint' };
  }
  try {
    const res = await fetchImpl(url);
    const text = await res.text();
    if (new TextEncoder().encode(text).byteLength > MAX_RAW_BYTES) {
      // 与实时腿同一条尺寸红线：过大的响应不是会话 JSON。
      return { ok: false, error: 'refused: response exceeds MAX_RAW_BYTES' };
    }
    return { ok: true, status: res.status, text };
  } catch (err) {
    // 只回技术细节，绝不回正文。
    return { ok: false, error: (err as Error).message };
  }
}

/**
 * 内容脚本的消息入口。返回 null 表示「这条消息不是给我的」，
 * 调用方就该让给别的监听器。把它抽成纯函数是为了能在 node 里测。
 */
export function handleBackfillMessage(
  message: unknown,
  pageOrigin: string,
  fetchImpl: FetchLike,
): Promise<BackfillFetchReply | { ok: true; origin: string }> | null {
  if (isBackfillPing(message)) return Promise.resolve({ ok: true as const, origin: pageOrigin });
  if (isBackfillFetchRequest(message)) return serveBackfillFetch(message.url, pageOrigin, fetchImpl);
  return null;
}

export type TabSend = (tabId: number, message: unknown) => Promise<unknown>;

/**
 * 把「某个活着的平台标签页」包成 engine 认得的 HttpPort。
 * 🔴 失败一律 throw：engine 会 halt('transport-error') 并留痕，
 *    绝不会静默地当成"取到了空数据"。
 */
export function tabHttpPort(tabId: number, send: TabSend): HttpPort {
  return async (url: string): Promise<HttpResponse> => {
    const reply = await send(tabId, { type: BACKFILL_FETCH_MESSAGE, url });
    if (!isRecord(reply)) {
      throw new Error(`tab ${tabId} gave no reply for the backfill fetch`);
    }
    if (reply.ok !== true) {
      throw new Error(String(reply.error ?? 'tab refused the backfill fetch'));
    }
    if (typeof reply.status !== 'number' || typeof reply.text !== 'string') {
      throw new Error(`tab ${tabId} replied with an unrecognised shape`);
    }
    return { status: reply.status, text: reply.text };
  };
}

// ---------------------------------------------------------------------------
// 活着的标签页登记表
//
// 为什么需要它：闹钟醒来时 SW 是【全新的】，没有任何内存态，也不知道用户
// 现在开着哪些页面。内容脚本每次加载都会报一次到（sender.tab.id 由浏览器填，
// 不需要 'tabs' 权限），background 把它记进 storage.local。闹钟醒来时按这张表
// 去 ping；ping 不通（标签页已经关了）就当作没有端口 —— 表会自己收敛。
// ---------------------------------------------------------------------------

function isTabEntry(v: unknown): v is TabEntry {
  return isRecord(v)
    && typeof v.tabId === 'number'
    && Number.isInteger(v.tabId)
    && typeof v.origin === 'string'
    && typeof v.at === 'number';
}

export async function loadTabs(store: BackfillStore | null): Promise<TabEntry[]> {
  if (!store) return [];
  const raw = await store.load(BACKFILL_TABS_KEY);
  return Array.isArray(raw) ? raw.filter(isTabEntry) : [];
}

/** 记一个（tabId 去重，最近的排最前）。 */
export async function rememberTab(store: BackfillStore | null, entry: TabEntry): Promise<TabEntry[]> {
  if (!store) return [];
  const rest = (await loadTabs(store)).filter((t) => t.tabId !== entry.tabId);
  const next = [entry, ...rest].slice(0, MAX_TAB_ENTRIES);
  await store.save(BACKFILL_TABS_KEY, next);
  return next;
}

export async function forgetTab(store: BackfillStore | null, tabId: number): Promise<void> {
  if (!store) return;
  const next = (await loadTabs(store)).filter((t) => t.tabId !== tabId);
  await store.save(BACKFILL_TABS_KEY, next);
}

/**
 * 挑一个真的还活着的标签页。`ping` 通不过就顺手把它从表里划掉。
 * origin = null 表示"任意平台都行"（Popup 问 transportWired 时用）。
 */
export async function pickLiveTab(
  store: BackfillStore | null,
  origin: string | null,
  ping: (tabId: number) => Promise<unknown>,
): Promise<TabEntry | null> {
  for (const entry of await loadTabs(store)) {
    if (origin !== null && entry.origin !== origin) continue;
    try {
      const reply = await ping(entry.tabId);
      if (isRecord(reply) && reply.ok === true) return entry;
    } catch {
      // 标签页已经关了 / 内容脚本没在：不是错误，是常态。
    }
    await forgetTab(store, entry.tabId);
  }
  return null;
}
