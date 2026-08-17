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
 * ## 自我约束（内容脚本侧，见 checkBackfillRequest / serveBackfillFetch）
 *  1. 只允许**同源**：请求 URL 的 origin 必须逐字等于页面自己的 origin；
 *  2. 只允许**平台表里的源**；
 *  3. 只允许**这个平台自己写过 plan** 的回溯（C22）；
 *  4. 只允许**回溯腿自己那两条路径**（会话列表 / 会话正文）。
 *  --- 🔴 C23 新增的三条：通道能发 POST 了，白名单必须跟着长出 method/body 这两维 ---
 *  5. method 必须落在**闭集** ALLOWED_BACKFILL_METHODS，**且**必须逐字等于
 *     plan 为这一段声明的那一个方法（expectedMethodFor）。plan 说 GET 的段
 *     发 POST ⇒ 拒；反之亦然。**请求自己说了不算。**
 *  6. GET 段**一律不许带 body / Content-Type**；
 *  7. POST 段的 body 必须：是字符串、≤ MAX_REQUEST_BODY_BYTES、
 *     JSON.parse 出来是**普通对象**、顶层键是 plan 声明的 bodyKeys 的**子集**、
 *     每个值只能是 string/number/boolean/null（不许嵌套对象或数组），
 *     Content-Type 必须逐字等于 plan 声明的那一个。
 * 任何一条不满足就拒绝并回错，绝不代发。
 *
 * ## 🔴 为什么这还不是「通用代理」
 * 见 checkBackfillRequest 头上的那段说明：可变的只剩下【一个闭集里的键】
 * 对应的【原始标量值】，而 URL、源、路径、方法、结构、体量全部被钉死。
 */

import { getPlatformByOrigin, MAX_RAW_BYTES } from '../contract';
import {
  backfillPlanFor,
  expectedMethodFor,
  postSpecFor,
  ALLOWED_BACKFILL_CONTENT_TYPES,
  ALLOWED_BACKFILL_METHODS,
  MAX_REQUEST_BODY_BYTES,
  type BackfillEnumPlan,
  type BackfillMethod,
  type BackfillRequestInit,
  type BackfillSegment,
} from './enumerate';
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

/**
 * 🔴 C23：消息现在可以带 method/body/contentType 三个**可选**字段。
 *    这里只做「形状认不认」的最浅判断，**真正的准入在 checkBackfillRequest**。
 *    三个字段都不带 ⇒ 与 C22 的消息逐字相同 ⇒ 走 GET 那条老路。
 */
export function isBackfillFetchRequest(
  v: unknown,
): v is { type: string; url: string; method?: unknown; body?: unknown; contentType?: unknown } {
  return isRecord(v) && v.type === BACKFILL_FETCH_MESSAGE && typeof v.url === 'string';
}

/** 把一条消息读成请求描述。非字符串的字段一律当成"没给"，交给白名单去拒。 */
function specFromMessage(m: { url: string; method?: unknown; body?: unknown; contentType?: unknown }): BackfillRequestSpec {
  return {
    url: m.url,
    method: typeof m.method === 'string' ? m.method : undefined,
    body: typeof m.body === 'string' ? m.body : undefined,
    contentType: typeof m.contentType === 'string' ? m.contentType : undefined,
  };
}

export function isBackfillPing(v: unknown): v is { type: string } {
  return isRecord(v) && v.type === BACKFILL_PING_MESSAGE;
}

export function isTabHello(v: unknown): v is { type: string; origin: string } {
  return isRecord(v) && v.type === BACKFILL_TAB_HELLO_MESSAGE && typeof v.origin === 'string';
}

/** 一次代发请求的完整描述。method 省略 ⇒ 'GET'（与 C22 的消息形状逐字兼容）。 */
export interface BackfillRequestSpec {
  url: string;
  method?: string;
  body?: string;
  contentType?: string;
}

/**
 * 🔴 C22 就存在的那句拒绝理由，**逐字保留**。
 * 它是回给 background 的 wire 值（tests/c19-runit.test.ts:433 在断言它），
 * 所以 URL 这一维的四条检查全部沿用它 —— 向后兼容不打折。
 * 更细的分类只进日志（verdict.detail），不改 wire。
 */
export const REFUSED_URL_REASON = 'refused: url is not a same-origin backfill endpoint';

export type RequestVerdict =
  | { ok: true; url: string; method: BackfillMethod; body?: string; contentType?: string }
  /** reason = 回给对端的（wire）；detail = 只进日志的细分。 */
  | { ok: false; reason: string; detail: string };

/** URL 这一维的拒绝：wire 值恒为 C22 那句，细分只进日志。 */
function refuseUrl(detail: string): { ok: false; reason: string; detail: string } {
  return { ok: false, reason: REFUSED_URL_REASON, detail };
}

/** 🔴 C23 新增的两维（method / body）：这些是新行为，可以有自己的 wire 理由。 */
function refuseRequest(reason: string): { ok: false; reason: string; detail: string } {
  return { ok: false, reason, detail: reason };
}

/** plan 查表函数。生产恒为 backfillPlanFor；参数化只是为了能拿合成 plan 做断言。 */
export type PlanLookup = (platform: string) => BackfillEnumPlan | null;

/** 请求 body 里允许出现的值类型：只有标量。不许嵌套对象/数组。 */
function isScalar(v: unknown): boolean {
  return v === null || typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean';
}

/**
 * 🔴 **C23 的安全落点。内容脚本敢不敢代发这一条请求。**
 *
 * ## 「GET 和 POST 在安全上不是一回事」这句话具体指什么
 * GET 只有 URL 一个自由度，而 URL 已经被同源 + 平台表 + 路径三重钉死。
 * POST 多了 body —— 一个**任意长度、任意结构、会被送到目标服务器**的字段。
 * 如果它不受约束，这个内容脚本就变成了「替我向 chatgpt.com 发任意 POST」的
 * 代理：拿着用户的登录 cookie，做用户没要求过的写操作。**那是不可接受的。**
 *
 * ## 因此 body 的自由度被压到这个程度
 *  · 谁能构造它：只有 plan 自己的 `spec.body()`（lib/backfill/enumerate.ts），
 *    engine 从来不接受外部传进来的 body（engine.ts 的 listRequestInit/detailRequestInit）；
 *  · 收到之后还要再验一遍：顶层键 ⊆ plan 声明的 bodyKeys（闭集），
 *    值只能是标量，整体 ≤ MAX_REQUEST_BODY_BYTES，Content-Type 必须逐字命中。
 *  · ⇒ 即使有人能往这个消息通道里塞东西，他能改的**只剩下闭集里那几个键的标量值**
 *    （对 Kimi 那种 plan 而言就是「翻页游标写成别的数」）。他不能换 URL、
 *    不能换源、不能换路径、不能换方法、不能加键、不能塞嵌套结构、不能塞大 payload。
 *
 * ## 谁能塞进来（判据 4 的正面回答）
 * 这个函数只被 `browser.runtime.onMessage` 那条路径调用
 *（entrypoints/dw-bridge.content.ts:161-176）。**页面 JS 够不着它**：
 * `browser.runtime` 只存在于 ISOLATED world，页面拿不到；manifest 里也没有
 * `externally_connectable`，所以任何网站都不能 sendMessage 进来。
 * 页面唯一能碰到的入口是同文件里的 `window.addEventListener('message')`，
 * 那条路径只产出 `chat-captured`，**与代发请求完全不相通**。
 */
export function checkBackfillRequest(
  spec: BackfillRequestSpec,
  pageOrigin: string,
  lookup: PlanLookup = backfillPlanFor,
): RequestVerdict {
  let u: URL;
  try {
    u = new URL(spec.url);
  } catch {
    return refuseUrl('url is not parseable');
  }
  if (u.origin !== pageOrigin) {                        // 1 · 同源
    return refuseUrl('url is not same-origin with the page');
  }
  const row = getPlatformByOrigin(u.origin);            // 2 · 在平台表里
  if (!row) return refuseUrl('origin is not in the platform table');
  const plan = lookup(row.id);                          // 3 · 这个平台真的能回溯
  if (!plan) return refuseUrl(`platform ${row.id} has no backfill plan`);

  // 4 · 只有它自己的那两条路径 —— 顺便定下这是哪一段（决定了允许的方法/body）。
  let segment: BackfillSegment;
  if (u.pathname === plan.listPath) segment = 'list';
  else if (u.pathname.startsWith(plan.detailPath)) segment = 'detail';
  else return refuseUrl('path is not a backfill endpoint');

  // 5 · method：先过闭集，再必须逐字等于 plan 为这一段声明的那一个。
  const method = spec.method ?? 'GET';
  if (!(ALLOWED_BACKFILL_METHODS as readonly string[]).includes(method)) {
    return refuseRequest(`refused: method ${sanitiseMethod(method)} is not in the allowed set`);
  }
  const expected = expectedMethodFor(plan, segment);
  if (method !== expected) {
    return refuseRequest(`refused: ${row.id} ${segment} segment must be ${expected}, got ${sanitiseMethod(method)}`);
  }

  const post = postSpecFor(plan, segment);
  if (!post) {
    // 6 · GET 段：一律不许带 body / Content-Type。
    if (spec.body !== undefined) return refuseRequest('refused: GET request must not carry a body');
    if (spec.contentType !== undefined) {
      return refuseRequest('refused: GET request must not carry a content-type');
    }
    return { ok: true, url: spec.url, method: 'GET' };
  }

  // 7 · POST 段的 body 闭集校验。
  if (typeof spec.body !== 'string') return refuseRequest('refused: POST request has no string body');
  if (new TextEncoder().encode(spec.body).byteLength > MAX_REQUEST_BODY_BYTES) {
    return refuseRequest('refused: request body exceeds MAX_REQUEST_BODY_BYTES');
  }
  if (spec.contentType !== post.contentType) {
    return refuseRequest('refused: content-type is not the one declared by the plan');
  }
  if (!(ALLOWED_BACKFILL_CONTENT_TYPES as readonly string[]).includes(spec.contentType)) {
    // plan 自己也不许声明闭集之外的 Content-Type。双保险：表写错了也发不出去。
    return refuseRequest('refused: content-type is not in the allowed set');
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(spec.body);
  } catch {
    return refuseRequest('refused: request body is not JSON');
  }
  if (!isRecord(parsed) || Array.isArray(parsed)) {
    return refuseRequest('refused: request body is not a JSON object');
  }
  const allowedKeys = new Set(post.bodyKeys);
  for (const [key, value] of Object.entries(parsed)) {
    if (!allowedKeys.has(key)) {
      // 🔴 只回「有个键不在闭集里」，不回键名本身：键名可能被塞进来当外泄载体。
      return refuseRequest('refused: request body has a key outside the declared allow-list');
    }
    if (!isScalar(value)) {
      return refuseRequest('refused: request body value is not a scalar');
    }
  }
  return { ok: true, url: spec.url, method: 'POST', body: spec.body, contentType: spec.contentType };
}

/** 拒绝理由里回显 method 时先截短 —— 免得一个超长字符串被原样带进日志。 */
function sanitiseMethod(method: string): string {
  return method.length > 16 ? `${method.slice(0, 16)}…` : method;
}

/**
 * 🔴 C22 留下的 URL 白名单，语义不变：**这个 URL 能不能用它那一段的默认方式发出去。**
 * 现在实现为 checkBackfillRequest 的一个特例（不传 method ⇒ 'GET'），
 * 于是「URL 白名单」和「method/body 白名单」不可能各说各话 —— 只有一份判断。
 */
export function isAllowedBackfillUrl(url: string, pageOrigin: string): boolean {
  return checkBackfillRequest({ url }, pageOrigin).ok;
}

export type FetchLike = (
  url: string,
  init?: BackfillRequestInit,
) => Promise<{ status: number; text: () => Promise<string> }>;

/**
 * 内容脚本侧的取数实现。**这段代码跑在用户已登录的那个页面的上下文里。**
 * 任何失败都变成 `{ok:false}`，绝不把异常抛回消息通道（抛出去会变成一句
 * 看不懂的 "Could not establish connection"）。
 */
export async function serveBackfillFetch(
  request: string | BackfillRequestSpec,
  pageOrigin: string,
  fetchImpl: FetchLike,
  lookup: PlanLookup = backfillPlanFor,
): Promise<BackfillFetchReply> {
  // 字符串形态是 C22 的调用方式，保留：等价于「这个 URL，用它那一段的默认方法」。
  const spec: BackfillRequestSpec = typeof request === 'string' ? { url: request } : request;
  const verdict = checkBackfillRequest(spec, pageOrigin, lookup);
  if (!verdict.ok) {
    // 🔴 留痕：拒发也要说得出口，而且与「取数失败」走同一个回错通道
    //    （{ok:false} → tabHttpPort throw → engine halt('transport-error') 落盘）。
    //    只打技术理由，绝不打 URL / body / 正文。
    console.warn(`[chat-stasher] backfill fetch refused: ${verdict.detail}`);
    return { ok: false, error: verdict.reason };
  }
  try {
    const init: BackfillRequestInit = verdict.method === 'POST'
      ? { method: 'POST', body: verdict.body, contentType: verdict.contentType as BackfillRequestInit['contentType'] }
      : { method: 'GET' };
    // 🔴 GET 段逐字维持 C22 的调用：只传 url，一个参数都不多传。
    const res = verdict.method === 'GET' ? await fetchImpl(verdict.url) : await fetchImpl(verdict.url, init);
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
  lookup: PlanLookup = backfillPlanFor,
): Promise<BackfillFetchReply | { ok: true; origin: string }> | null {
  if (isBackfillPing(message)) return Promise.resolve({ ok: true as const, origin: pageOrigin });
  if (isBackfillFetchRequest(message)) {
    return serveBackfillFetch(specFromMessage(message), pageOrigin, fetchImpl, lookup);
  }
  return null;
}

export type TabSend = (tabId: number, message: unknown) => Promise<unknown>;

/**
 * 把「某个活着的平台标签页」包成 engine 认得的 HttpPort。
 * 🔴 失败一律 throw：engine 会 halt('transport-error') 并留痕，
 *    绝不会静默地当成"取到了空数据"。
 */
export function tabHttpPort(tabId: number, send: TabSend): HttpPort {
  return async (url: string, init?: BackfillRequestInit): Promise<HttpResponse> => {
    // 🔴 向后兼容的落点：GET 且无 body ⇒ 发出去的消息**逐字**还是 `{type, url}`，
    //    一个字段都不多。C22 那条 ChatGPT 路径在线上看到的东西完全没变。
    const message = !init || (init.method === 'GET' && init.body === undefined)
      ? { type: BACKFILL_FETCH_MESSAGE, url }
      : { type: BACKFILL_FETCH_MESSAGE, url, method: init.method, body: init.body, contentType: init.contentType };
    const reply = await send(tabId, message);
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
