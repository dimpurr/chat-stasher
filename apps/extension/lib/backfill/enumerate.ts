/**
 * 枚举段：拿会话列表 + total。
 *
 * ## C22 · 这个文件从「一个平台的实现」变成「一张表 + 一份诚实的空缺清单」
 *
 * 修改前：这里只有 ChatGPT 那一套常量与解析器，engine 直接引用。
 * 后果不是「别的平台不动」，而是**别的平台会被拿 ChatGPT 的路径去打**：
 * engine 用 listPageUrl(origin) 拼出 `https://chat.deepseek.com/backend-api/conversations`，
 * 内容脚本的 isAllowedBackfillUrl 只比对路径、不比对平台，于是它会真的发出去，
 * 拿回 404 ⇒ halt('shape-changed')。用户看到的是「接口改了」，
 * 而真相是「我们压根没写过 DeepSeek 的列表入口」。这两句话对用户的含义完全不同。
 *
 * 所以现在：
 *  · 能回溯的平台 ⇒ 在 BACKFILL_PLANS 里有一条完整的 plan（下面那张表的每一项都齐）；
 *  · 不能回溯的平台 ⇒ 在 BACKFILL_UNSUPPORTED 里有一条【明写缺哪几项】的记录，
 *    engine 在发出任何请求【之前】就 halt('unsupported-platform')，Popup 照实说。
 *  · 平台表里的每一行必须恰好落在其中一侧 —— tests/c22-enumplat.test.ts 会盯着。
 *
 * ## 一个平台要被回溯，最少必须声明哪几项（= BackfillEnumPlan 的字段）
 *  1. listPath      列表入口的路径
 *  2. listUrl       分页方式（怎么把 offset/limit 变成一个【GET】URL）
 *  3. parseListPage 列表响应的形状判据：从哪儿取 ids、从哪儿取 total
 *  4. detailPath    正文入口的路径前缀
 *  5. detailUrl     会话 id → 正文 URL
 *  6. 正文形状判据  —— 不在这张表里：直接复用 lib/contract.ts 的 responseShape
 *                     （engine.ts:348 的 matchesResponseShape），实时腿与回溯腿同一把尺。
 *  7. provenance    出处，口径与 contract.ts 的 credibility 一致（源码 · 仓库/文件行 · license · 日期）
 *
 * 🔴 结构性约束（不是偏好，是当前接线的事实）：
 *    HttpPort 的签名是 `(url: string) => Promise<HttpResponse>`（engine.ts:37），
 *    只有 URL，没有 method/body ⇒ **只能 GET**。
 *    列表入口是 POST（例如 Kimi 的 Connect 风格 RPC，参数在 body 里）的平台，
 *    就算知道路由也接不上 —— 这一条在下面的 missing 里逐条写明，不含糊过去。
 *
 * ## 事实与复核状态（诚实标注，沿用本文件原有口径）
 *  · ChatGPT 的会话列表是 GET /backend-api/conversations?offset=&limit=，响应里自带 total。
 *    **这一条没有被复核** —— 本任务禁止真调平台接口，也没有登录态。
 *    所以 items / total 这两个字段名属于**待验证的假设**，不是实测结果。
 *  · 因此解析器写成「形状不合就报 shape-changed 并停」，而不是尽力猜。
 *    假设错了会立刻变成一条留痕的停机记录，不会变成静默爬不动，也不会变成假进度。
 *  · lib/contract.ts:114 登记的 chatgpt pathHints 是 '/backend-api/conversation/'
 *    （单数，取正文用）；列表是 '/backend-api/conversations'（复数），两者不同。
 */

import { PLATFORMS } from '../contract';

export const CHATGPT_LIST_PATH = '/backend-api/conversations';
export const CHATGPT_DETAIL_PATH = '/backend-api/conversation/';

/** 一页拿多少条。28 是列表接口常见的默认页大小；1000 条 ≈ 36 页仍属「便宜」。 */
export const DEFAULT_LIST_LIMIT = 100;

export interface EnumPage {
  ids: string[];
  /** 接口直给的总数；拿不到就是 null。 */
  total: number | null;
}

export type ParseResult =
  | { ok: true; page: EnumPage }
  | { ok: false; detail: string };

/**
 * 🔴 回溯一个平台所需的【最小声明集】。填满这七项就能被回溯；缺一项就不能。
 * 加一个平台 = 加一条这个结构，不需要动 engine 一行。
 */
export interface BackfillEnumPlan {
  /** 必须逐字等于 lib/contract.ts 平台表里的 id。 */
  platform: string;
  /** 1 · 列表入口（路径，用来给内容脚本做白名单比对，必须是精确路径）。 */
  listPath: string;
  /** 2 · 分页方式。只能产出 GET URL —— 见文件头的结构性约束。 */
  listUrl(origin: string, offset: number, limit: number): string;
  /** 3 · 列表响应的形状判据。不认识就返回 {ok:false}，engine 会 halt 留痕。 */
  parseListPage(text: string): ParseResult;
  /** 4 · 正文入口（路径前缀）。 */
  detailPath: string;
  /** 5 · 会话 id → 正文 URL。 */
  detailUrl(origin: string, conversationId: string): string;
  /** 7 · 出处。口径与 contract.ts 的 credibility 注释一致。 */
  provenance: string;
}

/**
 * 🔴 「这个平台暂时回溯不了」的**显式记录**。
 *
 * 为什么必须是一条数据而不是「表里没有它」：
 * 「没有历史」和「我们还不会读你的历史」对用户是完全不同的两件事。
 * 前者是枚举出 0 条，后者必须是一句「还没支持」。没有这条记录，
 * 两者在 UI 上会长得一模一样。
 */
export interface UnsupportedBackfill {
  platform: string;
  /** 已经有出处的部分。可能是空数组（= 什么都没查到）。 */
  known: readonly string[];
  /** 🔴 还缺哪几项。缺任意一项就填不了 —— 每一项都对应上面那七项之一。 */
  missing: readonly string[];
  /** 给用户看的一句话（Popup 用）。不含技术黑话，也不许暗示「它在补」。 */
  userNote: string;
}

// ---------------------------------------------------------------------------
// 列表解析器
// ---------------------------------------------------------------------------

/**
 * 解析一页会话列表（ChatGPT 形状）。
 * 严格：items 必须是数组、每个元素必须有 string 的 id；
 * total 只有是非负整数时才认，否则 total = null（⇒ 进度走「总数未知」分支）。
 */
export function parseConversationListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'list response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'list response is not a JSON object' };
  }
  const record = body as Record<string, unknown>;
  const items = record.items;
  if (!Array.isArray(items)) {
    return { ok: false, detail: 'list response has no `items` array' };
  }
  const ids: string[] = [];
  for (const item of items) {
    if (!item || typeof item !== 'object') {
      return { ok: false, detail: 'list item is not an object' };
    }
    const id = (item as Record<string, unknown>).id;
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, detail: 'list item has no string `id`' };
    }
    ids.push(id);
  }
  const rawTotal = record.total;
  const total =
    typeof rawTotal === 'number' && Number.isInteger(rawTotal) && rawTotal >= 0 ? rawTotal : null;
  return { ok: true, page: { ids, total } };
}

// ---------------------------------------------------------------------------
// 能回溯的平台
// ---------------------------------------------------------------------------

export const CHATGPT_PLAN: BackfillEnumPlan = {
  platform: 'chatgpt',
  listPath: CHATGPT_LIST_PATH,
  listUrl: (origin, offset, limit) =>
    `${origin}${CHATGPT_LIST_PATH}?offset=${offset}&limit=${limit}`,
  parseListPage: parseConversationListPage,
  detailPath: CHATGPT_DETAIL_PATH,
  detailUrl: (origin, id) => `${origin}${CHATGPT_DETAIL_PATH}${encodeURIComponent(id)}`,
  // 出处：**没有外部出处**。这一套是 C11 交给上一位 worker 的「已查到的事实」，
  // 当时就没有复核，本任务也没有复核（禁止联网、无登录态）。
  // 所以它的可信度等级是【待验证假设】，不是 'from-source'。
  // 之所以仍然保留为「能回溯」，是因为它已经带着完整的七项声明 + 形状判据：
  // 假设错了会立刻 halt('shape-changed') 留痕，不会变成假进度。
  provenance:
    'unverified-assumption · GET /backend-api/conversations?offset=&limit= with {items[].id, total};'
    + ' 无外部源码出处，未做真实端到端验证（本任务禁止联网与登录态）',
};

const PLANS: readonly BackfillEnumPlan[] = [CHATGPT_PLAN];

// ---------------------------------------------------------------------------
// 🔴 填不了 / 只能填一半的平台 —— 逐条写明缺什么
//
// 检索范围（诚实交代）：**只检索了本仓库内已经记录的出处**
// （lib/contract.ts 各行的 external source evidence 注释，那是上几位 worker 在
//  允许联网时留下的、带 commit / license / 日期的源码级引用）。
// 🔴 本任务【明令禁止发起任何真实网络请求】，所以我没有做任何联网检索：
//    没有打开 GitHub，没有打开任何平台页面，没有登录态。
//    因此下面每一条 missing 的含义都是「**在允许的检索范围内未找到**」，
//    而不是「不存在」—— 这两件事必须分开写。
// ---------------------------------------------------------------------------

export const BACKFILL_UNSUPPORTED: readonly UnsupportedBackfill[] = [
  {
    platform: 'deepseek',
    known: [
      // 列表入口有出处：lib/contract.ts:90-96 记录 deepseek-pp（Apache-2.0，
      // commit 0a02c72b135bf2936e11aa78fd6136931ed65908，2026-08-14）用到
      // /api/v0/chat_session/fetch_page，那正是「翻会话列表」的路由。
      'listPath 有出处：/api/v0/chat_session/fetch_page（lib/contract.ts:90-96 转引 deepseek-pp，Apache-2.0，2026-08-14）',
    ],
    missing: [
      'listUrl：分页参数名与请求方法未知（仓库内的引用只记了路由，没记 query/body 形状；fetch_page 这个名字暗示是分页，但参数名我没有出处）',
      'parseListPage：列表响应里会话数组与 total 的字段名未知（仓库内只记了 normalize 阶段要求 chat_sessions/chat_messages，那是导出器【规整之后】的形状，不是 wire 形状）',
    ],
    userNote: 'DeepSeek：还不能回溯历史。已经知道列表接口在哪，但不知道怎么翻页、也不知道返回里哪个字段是会话列表。',
  },
  {
    platform: 'claude',
    known: [
      // lib/contract.ts:148-153 明确写了 conversation-LIST 路由是 '/chat_conversations'
      // （不带尾斜杠那条），并在 176-184 记录了 claude-chat-exporter（MIT，
      // commit 12da324dd158e9472251590d89d957fc767c0d85，2026-08-08）请求的是
      // /api/organizations/<org>/chat_conversations/<uuid>。
      'listPath 有出处：/api/organizations/<org>/chat_conversations（lib/contract.ts:148-153、176-184 转引 claude-chat-exporter，MIT，2026-08-08）',
    ],
    missing: [
      'listUrl：路由里那个 <org> 组织 id 从哪来【没有出处】—— 它不在页面 URL 里，得先调另一个接口拿；那个接口我没有出处，编一个就是让用户以为在补历史',
      'listUrl：分页参数名未知',
      'parseListPage：列表响应的会话数组 / total 字段名未知（仓库内记录的 chat_messages 是【正文】那条路由的字段，不是列表的）',
    ],
    userNote: 'Claude：还不能回溯历史。列表接口的地址里有一段组织编号，我们没有可靠办法拿到它。',
  },
  {
    platform: 'kimi',
    known: [
      // lib/contract.ts:220-224 写明 conversation-INDEX 路由是 '.../ListChats'，
      // 且整个 ChatService 是 Connect 风格 unary RPC：POST + JSON body。
      'listPath 有出处：.../ChatService/ListChats（lib/contract.ts:220-224）',
      '请求形态有出处：Connect 风格 unary RPC = POST + JSON body（lib/contract.ts:226-229）',
    ],
    missing: [
      '🔴 结构性阻塞：列表入口是 POST，而回溯腿的 HttpPort 只有 url、没有 method/body（lib/backfill/engine.ts:37）—— 就算知道参数也发不出去',
      'listUrl：翻页游标在 body 里，字段名未知',
      'parseListPage：列表响应的会话数组 / total 字段名未知',
    ],
    userNote: 'Kimi：还不能回溯历史。它的列表接口要用一种我们这条腿目前发不出去的请求方式。',
  },
  {
    platform: 'gemini',
    known: [],
    missing: [
      'listPath：❌ 未找到出处。检索范围 = 本仓库全部代码与注释（lib/contract.ts 的 gemini 行只有 /_/BardChatUi/data/batchexecute 这一条 RPC 端点，没有任何「会话列表」的记录）；未做联网检索（本任务禁止）',
      'listUrl / parseListPage：batchexecute 是把 RPC id 和参数编进 body、响应是分块的 ")]}\'" 前缀文本，既不是 GET 也不是 JSON —— 即使找到 RPC id，也同样撞上「HttpPort 只能 GET」这堵墙',
      'detailUrl：同上',
    ],
    userNote: 'Gemini：还不能回溯历史。我们没有找到可靠的「列出历史对话」接口出处，不会去猜一个。',
  },
];

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

/** 能回溯就返回 plan，否则 null。 */
export function backfillPlanFor(platform: string): BackfillEnumPlan | null {
  return PLANS.find((p) => p.platform === platform) ?? null;
}

/** 明确登记为「暂时回溯不了」的那条记录；不在清单里就是 null。 */
export function unsupportedBackfillFor(platform: string): UnsupportedBackfill | null {
  return BACKFILL_UNSUPPORTED.find((u) => u.platform === platform) ?? null;
}

/** 能回溯的平台 id（按平台表顺序）。 */
export const BACKFILL_SUPPORTED_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => backfillPlanFor(id) !== null);

/** 暂时回溯不了的平台 id（按平台表顺序）。 */
export const BACKFILL_UNSUPPORTED_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => backfillPlanFor(id) === null);

// ---------------------------------------------------------------------------
// back-compat：ChatGPT 的两个 URL 构造器。既有测试与接线仍在用。
// ---------------------------------------------------------------------------

export function listPageUrl(origin: string, offset: number, limit = DEFAULT_LIST_LIMIT): string {
  return CHATGPT_PLAN.listUrl(origin, offset, limit);
}

export function detailUrl(origin: string, conversationId: string): string {
  return CHATGPT_PLAN.detailUrl(origin, conversationId);
}
