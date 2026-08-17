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
 *  4. detailPath    正文入口的路径前缀（🔴 C26：允许 null = 正文段还没有出处）
 *  5. detailUrl     会话 id → 正文 URL（同上，允许 null）
 *  6. 正文形状判据  —— 不在这张表里：直接复用 lib/contract.ts 的 responseShape
 *                     （engine.ts:348 的 matchesResponseShape），实时腿与回溯腿同一把尺。
 *  7. provenance    出处，口径与 contract.ts 的 credibility 一致（源码 · 仓库/文件行 · license · 日期）
 *
 * 🔴 C23 之前的结构性约束（已解除，保留原文以便对照）：
 *    ~~HttpPort 的签名是 `(url: string) => Promise<HttpResponse>`（engine.ts:37），
 *    只有 URL，没有 method/body ⇒ **只能 GET**。~~
 *    C23 把 HttpPort 扩成 `(url, init?: BackfillRequestInit)`，plan 现在可以用
 *    `listPost` / `detailPost` 声明「这一段是 POST，body 长这样，顶层键只有这几个」。
 *    🔴 **但本任务【没有】给任何平台填这个声明** —— kimi / gemini 的列表形状
 *    仍然是「未找到出处 / 只有一半」，填了就是编。所以下面的 missing 里，
 *    「结构性阻塞」那一条被改写成「通道已经能发，但参数仍然没有出处」。
 *
 * ## 事实与复核状态（诚实标注，沿用本文件原有口径）
 *  · ChatGPT 的会话列表是 GET /backend-api/conversations?offset=&limit=，响应里自带 total。
 *    **这一条没有被复核** —— 本任务禁止真调平台接口，也没有登录态。
 *    所以 items / total 这两个字段名属于**待验证的假设**，不是实测结果。
 *  · 因此解析器写成「形状不合就报 shape-changed 并停」，而不是尽力猜。
 *    假设错了会立刻变成一条留痕的停机记录，不会变成静默爬不动，也不会变成假进度。
 *  · lib/contract.ts:114 登记的 chatgpt pathHints 是 '/backend-api/conversation/'
 *    （单数，取正文用）；列表是 '/backend-api/conversations'（复数），两者不同。
 *
 * ## C26 · DeepSeek 那一格从「未知」变成「有出处」，并顺手长出两样新东西
 *
 *  1. **游标式翻页**（listCursorUrl / EnumPage.nextCursor / EnumPage.hasMore）。
 *     C22 的 plan 只会 offset 翻页，因为当时表里只有 ChatGPT。DeepSeek 是
 *     `count` + `before_seq_id`（游标 = 上一页里最小的 seq_id），offset 那套在它身上
 *     压根不成立（page/offset/limit 在五个来源里一次都没出现）。
 *     🔴 这条分支的每一处「读不到」都落在具名结局上，见 types.ts 的 EnumTruncation：
 *     读不到 seq_id ⇒ 'cursor-missing'（只回溯到这一页，**不是**枚举完）；
 *     读不到 has_more ⇒ 'has-more-missing'（**不许**当成没有下一页）。
 *
 *  2. **半条腿是可以被写出来的**（detailPath/detailUrl 允许 null + partial）。
 *     DeepSeek 的列表段有四源交叉的出处，正文段没有。以前这种情况只能二选一：
 *     要么整个平台继续记成「不支持」（明明列表已经会读了），
 *     要么编一个正文路由把 plan 填满（那才是真正危险的那种谎）。
 *     现在它有第三种写法，并且对应一个独立的 halt 理由 'detail-unsupported'——
 *     它与 'unsupported-platform' 的差别是：后者一个请求都没发过。
 *
 *  🔴 与 C22 一样，本任务同样【没有联网、没有登录态、没有对 deepseek.com 发过任何请求】。
 *     DeepSeek 那格的出处来自 R25 调研单的**多源交叉**，不是官方文档，也未经实测复核 ——
 *     完整的「知道什么 / 不知道什么 / 时效风险」写在 DEEPSEEK_PLAN 头上，不许只留结论。
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
  /**
   * 🔴 C26 · 游标式翻页专用：下一页的游标。
   * `null` = **本页没能给出游标**（记录里没有游标字段）⇒ engine 只能停在这一页，
   * 并把 enumCursor.truncated 记成 'cursor-missing'。
   * `undefined` = 这个平台压根不是游标翻页（ChatGPT），engine 不看这一项。
   */
  nextCursor?: number | null;
  /**
   * 🔴 C26 · 接口自报「还有没有下一页」。
   * `undefined` = 响应里没有这个信号 ⇒ engine **不许当成 false**，
   * 只能停下并记 'has-more-missing'。
   */
  hasMore?: boolean;
  /**
   * 🔴 C26 · 本页里最新的一条更新时间戳，**原样的数值**，不做任何时区/格式转换。
   * 拿不到就是 null。写在这里是为了让「updated_at 是数值不是 ISO 串」这件事
   * 有一个可断言的落点 —— 见 parseDeepSeekListPage 的说明。
   */
  newestUpdatedAt?: number | null;
}

export type ParseResult =
  | { ok: true; page: EnumPage }
  | { ok: false; detail: string };

// ---------------------------------------------------------------------------
// 🔴 C23 · 通道能力的【闭集声明】
//
// 这一段是「让通道能表达 POST」与「不让通道变成通用代理」的同一个落点：
// method、Content-Type、body 的顶层键 —— 三样东西都在这里写成**枚举**，
// 内容脚本侧（lib/backfill/tab-port.ts 的 checkBackfillRequest）逐条比对。
//
// 为什么闭集必须长在 plan 上、而不是长在消息里：
// 消息是「谁来问」，plan 是「我们自己写过什么」。只有后者能当白名单 ——
// 白名单如果来自请求本身，那就不是白名单，是自证。
// ---------------------------------------------------------------------------

/** 🔴 允许的 HTTP 方法。**闭集**，只有这两个。 */
export const ALLOWED_BACKFILL_METHODS = ['GET', 'POST'] as const;
export type BackfillMethod = (typeof ALLOWED_BACKFILL_METHODS)[number];

/** 🔴 允许的请求 Content-Type。**闭集**，只有一个。 */
export const ALLOWED_BACKFILL_CONTENT_TYPES = ['application/json'] as const;
export type BackfillContentType = (typeof ALLOWED_BACKFILL_CONTENT_TYPES)[number];

/**
 * 🔴 请求 body 的字节上限。回溯腿的 body 只可能是「翻页游标 / 会话 id」这种
 * 几十字节的东西；给到 4 KiB 已经宽得离谱。这条线不是为了省流量，
 * 是为了让「借这个通道往外送东西」在体量上先不成立。
 */
export const MAX_REQUEST_BODY_BYTES = 4096;

/** 一次回溯请求除 URL 之外的全部可变量。省略 = GET 且无 body。 */
export interface BackfillRequestInit {
  method: BackfillMethod;
  /** 只有 POST 段才允许有；且必须过 bodyKeys 闭集校验。 */
  body?: string;
  contentType?: BackfillContentType;
}

/**
 * 🔴 某一段（列表 / 正文）是 POST 时，必须一次性声明清楚的三件事。
 * 缺任意一项就不是一条合法的 POST 声明 —— 也就发不出去。
 */
export interface BackfillPostSpec<Args extends unknown[]> {
  contentType: BackfillContentType;
  /**
   * 🔴 body 顶层允许出现的键。**闭集。**
   * 内容脚本会 JSON.parse 收到的 body，逐个顶层键比对：
   * 多出一个键就整条拒发。值只允许 string / number / boolean / null
   * —— 不许嵌套对象或数组，免得「一个闭集的键」底下挂一整棵任意结构。
   */
  bodyKeys: readonly string[];
  /** 我们【自己】构造 body 的那个函数。生产路径上 body 只可能出自这里。 */
  body(...args: Args): string;
}

/** 列表段的 POST 声明形状。 */
export type ListPostSpec = BackfillPostSpec<[origin: string, offset: number, limit: number]>;
/** 正文段的 POST 声明形状。 */
export type DetailPostSpec = BackfillPostSpec<[origin: string, conversationId: string]>;

export type BackfillSegment = 'list' | 'detail';

/**
 * 🔴 回溯一个平台所需的【最小声明集】。填满这七项就能被回溯；缺一项就不能。
 * 加一个平台 = 加一条这个结构，不需要动 engine 一行。
 */
export interface BackfillEnumPlan {
  /** 必须逐字等于 lib/contract.ts 平台表里的 id。 */
  platform: string;
  /** 1 · 列表入口（路径，用来给内容脚本做白名单比对，必须是精确路径）。 */
  listPath: string;
  /**
   * 2 · 分页方式（URL 部分）。
   * 🔴 C23：**不再等于「只能 GET」**。省略 listPost ⇒ 这一段是 GET，
   *    翻页参数全在 query 里（ChatGPT 就是这样，行为一个字不变）；
   *    声明了 listPost ⇒ 这一段是 POST，翻页参数在 body 里，这里只给路由。
   */
  listUrl(origin: string, offset: number, limit: number): string;
  /** 2b · 🔴 C23 新增（可选）。声明了就代表列表段用 POST 发。 */
  listPost?: ListPostSpec;
  /**
   * 2c · 🔴 C26 新增（可选）· **游标式翻页**。
   *
   * 声明了它就代表这个平台【不是 offset 翻页】：下一页要从上一页的内容里读出一个游标
   * （DeepSeek 是 `before_seq_id` = 上一页里最小的 `seq_id`）。engine 于是走游标那条分支，
   * listUrl 在这条分支上【一次都不会被调用】—— 但接口仍然要求填 listUrl，
   * 因为它同时是「offset 语义下这个平台长什么样」的书面记录，也是 back-compat 的落点。
   *
   * cursor === null ⇒ 第一页（还没有游标）。
   */
  listCursorUrl?(origin: string, cursor: number | null, limit: number): string;
  /** 3 · 列表响应的形状判据。不认识就返回 {ok:false}，engine 会 halt 留痕。 */
  parseListPage(text: string): ParseResult;
  /**
   * 4 · 正文入口（路径前缀）。
   * 🔴 C26：允许为 **null** —— 「列表段有出处、正文段还没有」是一个真实存在的中间态
   *    （DeepSeek 就是），它必须能被写出来，而不是逼着人去编一个正文路由。
   *    null ⇒ 内容脚本不会为这个平台放行任何正文 URL（tab-port.ts 的第 4 条），
   *    engine 也会在发出任何一条正文请求【之前】halt('detail-unsupported')。
   */
  detailPath: string | null;
  /** 5 · 会话 id → 正文 URL。🔴 C26：与 detailPath 同生共死，要么都有，要么都是 null。 */
  detailUrl: ((origin: string, conversationId: string) => string) | null;
  /** 5b · 🔴 C23 新增（可选）。声明了就代表正文段用 POST 发。 */
  detailPost?: DetailPostSpec;
  /**
   * 6b · 🔴 C26（可选）· 这条 plan **只覆盖了一半**时，缺的那一半写在这里。
   * 口径与 UnsupportedBackfill 完全一致（missing + 给用户的一句人话），
   * 因为它回答的是同一个问题：「你到底还有哪儿不会」。
   * 没有这个字段 = 这条 plan 是完整的（列表 + 正文都能走）。
   */
  partial?: PartialBackfill;
  /** 7 · 出处。口径与 contract.ts 的 credibility 注释一致。 */
  provenance: string;
}

/** 🔴 C26 · 「列表能列、正文还取不到」这种半条腿的显式记录。 */
export interface PartialBackfill {
  /** 还缺哪几项（对应上面那七项之一）。 */
  missing: readonly string[];
  /** 给用户看的一句话。不含技术黑话，也不许暗示「它在补」。 */
  userNote: string;
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

/**
 * 解析一页 DeepSeek 会话列表。
 *
 * ## 🔴 这个函数是「带着『它可能是错的』去写」的落点
 * 下面认的那几个字段名不是官方文档，是 R25 从**四个互不相干的开源实现**里交叉出来的
 * （出处见 DEEPSEEK_PLAN.provenance）。逆向来的形状随时可能被平台改掉。
 * 所以每一处「读不到」都必须落到一个**具名的、不同于「空」的**结局上：
 *
 *  · 读不到 `data.biz_data.chat_sessions` ⇒ `{ok:false}` ⇒ engine halt('shape-changed')。
 *    🔴 **绝不返回 `{ok:true, ids:[]}`** —— 那会让回溯腿以为「这个用户没有会话」，
 *    把 enumCursor 标成 complete，然后安安静静地宣布自己干完了。
 *    「接口变了」和「你没有历史」对用户是完全不同的两句话。
 *  · 读不到 `seq_id` ⇒ 形状还认得，只是**翻不了页** ⇒ nextCursor=null，
 *    engine 停在这一页并记 truncated='cursor-missing'（≠ 枚举完）。
 *  · 读不到 `has_more` ⇒ hasMore=undefined，engine 记 'has-more-missing' 后停，
 *    同样【不许】当成「没有下一页」。
 *
 * ## `updated_at` 是**数值**，不是 ISO 串
 * 三个源都显示它是数值型时间戳。所以这里：
 *  · 是有限数值 ⇒ 原样收下（不 new Date、不换算单位、不猜秒还是毫秒）；
 *  · **存在但不是数值**（例如变成了 ISO 串）⇒ 直接判 `{ok:false}`。
 *    这一条是故意的：字段类型变了就是 wire 形状变了，与其用 `new Date(string)`
 *    把它「宽容」过去，不如当场停下留痕。
 *  · 整个字段缺席 ⇒ 容忍（枚举并不需要它），newestUpdatedAt=null。
 */
export function parseDeepSeekListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'deepseek list response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'deepseek list response is not a JSON object' };
  }
  // 信封：data.biz_data（5 源一致）。🔴 顶层业务码（code / biz_code）两源打架，**不看它**。
  const data = (body as Record<string, unknown>).data;
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    return { ok: false, detail: 'deepseek list response has no `data` object (envelope changed?)' };
  }
  const biz = (data as Record<string, unknown>).biz_data;
  if (!biz || typeof biz !== 'object' || Array.isArray(biz)) {
    return { ok: false, detail: 'deepseek list response has no `data.biz_data` object (envelope changed?)' };
  }
  const bizRecord = biz as Record<string, unknown>;
  const sessions = bizRecord.chat_sessions;
  if (!Array.isArray(sessions)) {
    // 🔴 就是这一行在守「不能把未知当成空」：没有这个数组 = 形状变了，不是没有会话。
    return { ok: false, detail: 'deepseek list response has no `data.biz_data.chat_sessions` array (shape changed?)' };
  }

  const ids: string[] = [];
  let minSeqId: number | null = null;
  let seqIdMissing = false;
  let newestUpdatedAt: number | null = null;

  for (const item of sessions) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) {
      return { ok: false, detail: 'deepseek chat_sessions item is not an object' };
    }
    const record = item as Record<string, unknown>;
    const id = record.id;
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, detail: 'deepseek chat_sessions item has no string `id`' };
    }
    ids.push(id);

    const seqId = record.seq_id;
    if (typeof seqId === 'number' && Number.isFinite(seqId)) {
      minSeqId = minSeqId === null ? seqId : Math.min(minSeqId, seqId);
    } else {
      // 🔴 有一条读不出游标，整页的游标就不可信了：宁可停，不许翻错页漏掉会话。
      seqIdMissing = true;
    }

    const updatedAt = record.updated_at;
    if (updatedAt !== undefined && updatedAt !== null) {
      if (typeof updatedAt !== 'number' || !Number.isFinite(updatedAt)) {
        return {
          ok: false,
          detail: 'deepseek chat_sessions item has a non-numeric `updated_at` (wire shape changed?)',
        };
      }
      newestUpdatedAt = newestUpdatedAt === null ? updatedAt : Math.max(newestUpdatedAt, updatedAt);
    }
  }

  const rawHasMore = bizRecord.has_more;
  return {
    ok: true,
    page: {
      ids,
      // 🔴 DeepSeek 的列表响应里**没有**总数字段的出处 ⇒ total 恒 null ⇒
      //    进度那边走「总数未知」分支，绝不显示百分比。不许拿 ids.length 冒充分母。
      total: null,
      nextCursor: seqIdMissing ? null : minSeqId,
      hasMore: typeof rawHasMore === 'boolean' ? rawHasMore : undefined,
      newestUpdatedAt,
    },
  };
}

// ---------------------------------------------------------------------------
// 能回溯的平台
// ---------------------------------------------------------------------------

/** ChatGPT 的正文 URL 构造器。抽成常量只是为了 back-compat 的 detailUrl() 能复用它。 */
const chatgptDetailUrl = (origin: string, id: string): string =>
  `${origin}${CHATGPT_DETAIL_PATH}${encodeURIComponent(id)}`;

export const CHATGPT_PLAN: BackfillEnumPlan = {
  platform: 'chatgpt',
  listPath: CHATGPT_LIST_PATH,
  listUrl: (origin, offset, limit) =>
    `${origin}${CHATGPT_LIST_PATH}?offset=${offset}&limit=${limit}`,
  parseListPage: parseConversationListPage,
  detailPath: CHATGPT_DETAIL_PATH,
  detailUrl: chatgptDetailUrl,
  // 出处：**没有外部出处**。这一套是 C11 交给上一位 worker 的「已查到的事实」，
  // 当时就没有复核，本任务也没有复核（禁止联网、无登录态）。
  // 所以它的可信度等级是【待验证假设】，不是 'from-source'。
  // 之所以仍然保留为「能回溯」，是因为它已经带着完整的七项声明 + 形状判据：
  // 假设错了会立刻 halt('shape-changed') 留痕，不会变成假进度。
  provenance:
    'unverified-assumption · GET /backend-api/conversations?offset=&limit= with {items[].id, total};'
    + ' 无外部源码出处，未做真实端到端验证（本任务禁止联网与登录态）',
};

export const DEEPSEEK_LIST_PATH = '/api/v0/chat_session/fetch_page';

/**
 * 🔴 C26 · DeepSeek 的会话列表。**只有列表段**；正文段仍然没有出处（见 partial）。
 *
 * ## 这一格是怎么从「未知」变成「有出处」的
 * C22 时这里写着「分页参数名与请求方法未知，我没有出处」。R25 调研单（2026-08-17）
 * 把出处补上了，且**每一项都是多源交叉**、不是单源孤证：
 *
 *   列表接口   GET https://chat.deepseek.com/api/v0/chat_session/fetch_page  · 4 源
 *   每页条数   count                                                          · 2 源
 *   翻页游标   before_seq_id，值 = 上一页里【最小】的 seq_id                   · 2 源（取法一致）
 *   响应信封   data.biz_data                                                  · 5 源
 *   列表数组   biz_data.chat_sessions                                         · 4 源
 *   还有下页   biz_data.has_more（boolean，与数组同级）                        · 3 源
 *   记录·id    id                                                             · 3 源
 *   记录·标题  title                                                          · 2 源
 *   记录·时间  updated_at 🔴 数值型时间戳，不是 ISO 串                          · 3 源
 *   记录·游标  seq_id（数值）                                                  · 2 源
 *
 * 其中路由这一条与本仓库 lib/contract.ts:90-96 已登记的 from-source 证据
 *（deepseek-pp，Apache-2.0，commit 0a02c72b…，2026-08-14）互相印证。
 *
 * ## 🔴 同样重要：**没有**出处的东西一个都没写进来
 *  · `count` 的服务端上限/默认值 —— 未找到 ⇒ 这里只发我们自己选的 DEFAULT_LIST_LIMIT，
 *    **绝不把 200 之类的数字当成上限写死**，也绝不假设「返回条数 < count ⇒ 最后一页」
 *    （那是拿未知当已知）。是不是最后一页只认 has_more。
 *  · 完整响应字段清单 —— 没有任何一份公开的完整 JSON 样例 ⇒ 解析器只认它用得上的那几个键，
 *    多出来的键一律不管（不当成形状变了）。
 *  · pinned / inserted_at / title_type / model_type —— 单源或防御式写法 ⇒ 不读。
 *  · 顶层业务码叫 code 还是 biz_code —— 两源打架 ⇒ **不依赖**（parseDeepSeekListPage 里
 *    一次都没有读过它）。
 *  · lte_cursor.updated_at / lte_cursor.pinned —— 一源且与另两源矛盾 ⇒ 排除。
 *  · page / offset / cursor / limit / page_size —— 五个来源里一次都没出现 ⇒ 排除。
 *
 * ## ⚠️ 时效与风险（照实写，不许粉饰）
 * 响应形状有 2026-08-17 的旁证；但**分页参数的实测证据最新只到 2025-12**，
 * 且那位作者提到 DeepSeek 已上线原生会话搜索 ⇒ 这个接口最近很可能动过。
 * 因此本 plan 的可信度是【多源交叉的逆向结论】，不是官方契约：
 * 猜错的后果被压在 parseDeepSeekListPage 的三条具名结局里
 *（shape-changed / cursor-missing / has-more-missing），不会变成假进度。
 */
export const DEEPSEEK_PLAN: BackfillEnumPlan = {
  platform: 'deepseek',
  listPath: DEEPSEEK_LIST_PATH,
  // 🔴 offset 语义在 DeepSeek 上【不成立】（page/offset/limit 五源皆无）。
  //    这里仍然给出一个 listUrl 是因为接口要求它必填，但 engine 走的是 listCursorUrl
  //    那条分支（声明了 listCursorUrl ⇒ 游标翻页），listUrl 一次都不会被调用。
  //    它只发第一页 —— 万一将来有人误用，拿到的也是「第一页」这个安全的东西，
  //    而不是一个我们编出来的 offset 参数。
  listUrl: (origin, _offset, limit) => `${origin}${DEEPSEEK_LIST_PATH}?count=${limit}`,
  listCursorUrl: (origin, cursor, limit) =>
    cursor === null
      ? `${origin}${DEEPSEEK_LIST_PATH}?count=${limit}`
      : `${origin}${DEEPSEEK_LIST_PATH}?count=${limit}&before_seq_id=${cursor}`,
  parseListPage: parseDeepSeekListPage,
  // 🔴 正文段：**没有出处，所以是 null**，不是「先随便填一个」。
  //    详见 partial.missing。
  detailPath: null,
  detailUrl: null,
  partial: {
    missing: [
      'detailPath / detailUrl：取【单条会话正文】的路由与参数没有多源出处。'
      + '仓库内 lib/contract.ts:90-96 记过 /api/v0/chat/history_messages 这个名字，'
      + '但它要哪些参数、正文是不是也要翻页（翻页参数又叫什么）都没有出处 —— '
      + '猜一个的后果不是报错，而是每条对话只存下前几轮、用户还以为存全了。',
    ],
    userNote:
      'DeepSeek：还不能回溯历史正文。已经能列出你的历史会话了，'
      + '但还不会去取每条会话的内容，所以暂时一条也不会存下来。',
  },
  provenance:
    'cross-source reverse-engineering (R25 调研单, 2026-08-17；四个互不相干的开源实现交叉一致) · '
    + 'GET /api/v0/chat_session/fetch_page?count=&before_seq_id= · 4 源；'
    + 'count 2 源；before_seq_id(=上一页最小 seq_id) 2 源；data.biz_data 5 源；'
    + 'chat_sessions 4 源；has_more 3 源；id 3 源；seq_id 2 源；updated_at(数值) 3 源。'
    + '路由与 lib/contract.ts:90-96 的 from-source 证据（deepseek-pp, Apache-2.0, '
    + 'commit 0a02c72b135bf2936e11aa78fd6136931ed65908, 2026-08-14）互相印证。'
    + '🔴 非官方文档；分页参数的实测证据最新只到 2025-12，接口可能已改动。'
    + '本任务未做任何真实端到端验证（禁止联网与登录态）。',
};

const PLANS: readonly BackfillEnumPlan[] = [DEEPSEEK_PLAN, CHATGPT_PLAN];

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

// 🔴 C26 · deepseek 这一条【被移走了】，不是被删掉不管了：
//    R25 调研单补齐了列表段的多源出处，所以它现在是 DEEPSEEK_PLAN 的一条 plan
//    （列表段完整 + partial 里写明正文段仍然缺什么）。
//    C22 定下的规矩没有松：平台表里每一行仍然恰好落在「有 plan」或「登记为暂时不能」
//    其中一侧，tests/c22-enumplat.test.ts 仍然盯着。
export const BACKFILL_UNSUPPORTED: readonly UnsupportedBackfill[] = [
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
      // 🔴 C23 前这里写的是「结构性阻塞：HttpPort 只有 url，就算知道参数也发不出去」。
      //    通道已经扩好了（listPost + BackfillRequestInit），那堵墙没了。
      //    但**剩下的两条一个字都没少** —— 参数仍然没有出处，所以仍然填不了。
      'listPost.body：翻页游标在 body 里，字段名未知（通道已能发 POST，但我们不知道该发什么；编一个就是让用户以为在补历史）',
      'listPost.bodyKeys：同上 —— 顶层键闭集必须来自出处，不能靠猜',
      'parseListPage：列表响应的会话数组 / total 字段名未知',
    ],
    userNote: 'Kimi：还不能回溯历史。我们不知道它的列表接口要什么参数，也不会去猜一个。',
  },
  {
    platform: 'gemini',
    known: [],
    missing: [
      'listPath：❌ 未找到出处。检索范围 = 本仓库全部代码与注释（lib/contract.ts 的 gemini 行只有 /_/BardChatUi/data/batchexecute 这一条 RPC 端点，没有任何「会话列表」的记录）；未做联网检索（本任务禁止）',
      // 🔴 C23：POST 这一半的墙拆了（见 listPost），但 batchexecute 还有另外两半：
      //    ① RPC id 没有出处；② 响应是 ")]}'" 前缀的分块文本，不是 JSON，
      //    parseListPage 得另写一个解析器，而写它同样需要出处。
      'listUrl / parseListPage：batchexecute 把 RPC id 和参数编进 body，响应是分块的 ")]}\'" 前缀文本而非 JSON —— C23 之后通道能发 POST 了，但 RPC id 与响应分块格式仍无出处',
      'listPost.contentType：batchexecute 的请求 Content-Type 未找到出处（仓库内没有记录，本任务禁止联网检索）。本通道的 Content-Type 闭集目前只有 application/json（ALLOWED_BACKFILL_CONTENT_TYPES）—— 若它不是 json，还得先【有出处地】扩这个闭集',
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

/**
 * 🔴 C23 · 某一段的 POST 声明。没有就是 null（= 这一段是 GET）。
 * **engine 与内容脚本用的是同一个函数**，所以「发出去的」和「被允许的」
 * 不可能各说各话 —— 白名单不是另抄一份，是同一份。
 */
export function postSpecFor(
  plan: BackfillEnumPlan,
  segment: BackfillSegment,
): ListPostSpec | DetailPostSpec | null {
  return (segment === 'list' ? plan.listPost : plan.detailPost) ?? null;
}

/** 🔴 某一段【唯一允许】的方法。plan 说了算，请求说了不算。 */
export function expectedMethodFor(plan: BackfillEnumPlan, segment: BackfillSegment): BackfillMethod {
  return postSpecFor(plan, segment) ? 'POST' : 'GET';
}

/** 列表段的完整请求参数。没有 listPost ⇒ `{method:'GET'}`，与 C22 逐字一致。 */
export function listRequestInit(
  plan: BackfillEnumPlan,
  origin: string,
  offset: number,
  limit: number,
): BackfillRequestInit {
  const spec = plan.listPost;
  if (!spec) return { method: 'GET' };
  return { method: 'POST', body: spec.body(origin, offset, limit), contentType: spec.contentType };
}

/** 正文段的完整请求参数。没有 detailPost ⇒ `{method:'GET'}`，与 C22 逐字一致。 */
export function detailRequestInit(
  plan: BackfillEnumPlan,
  origin: string,
  conversationId: string,
): BackfillRequestInit {
  const spec = plan.detailPost;
  if (!spec) return { method: 'GET' };
  return { method: 'POST', body: spec.body(origin, conversationId), contentType: spec.contentType };
}

/** 明确登记为「暂时回溯不了」的那条记录；不在清单里就是 null。 */
export function unsupportedBackfillFor(platform: string): UnsupportedBackfill | null {
  return BACKFILL_UNSUPPORTED.find((u) => u.platform === platform) ?? null;
}

/**
 * 🔴 C26 · 「这条 plan 能不能真的把历史【正文】补回来」。
 * 只有列表段的 plan（DeepSeek）在这一问上必须回答 false —— 它列得出会话，
 * 但一条正文都取不到，对用户而言历史仍然没补回来。
 */
export function canBackfillDetail(plan: BackfillEnumPlan): boolean {
  return plan.detailPath !== null && plan.detailUrl !== null;
}

/**
 * 能把历史**补回来**的平台 id（按平台表顺序）。
 * 🔴 判据是「列表 + 正文都能走」，不是「有没有 plan」——
 *    只能列出会话、取不到正文的平台【不算】能补回历史，否则 Popup 那一行会说谎。
 */
export const BACKFILL_SUPPORTED_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => {
    const plan = backfillPlanFor(id);
    return plan !== null && canBackfillDetail(plan);
  });

/**
 * 🔴 C26 新增 · **只列得出会话、还取不到正文**的平台 id。
 * 它既不属于「能补回历史」，也不等于「什么都不会」——
 * 中间态必须有自己的名字，否则只能被四舍五入成其中一边。
 */
export const BACKFILL_LIST_ONLY_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => {
    const plan = backfillPlanFor(id);
    return plan !== null && !canBackfillDetail(plan);
  });

/** 暂时补不回历史的平台 id（按平台表顺序）。🔴 含上面那些「只能列出」的。 */
export const BACKFILL_UNSUPPORTED_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => !BACKFILL_SUPPORTED_PLATFORMS.includes(id));

/** 只有一半的 plan 的那半条腿缺什么（按平台表顺序）。给 Popup 用。 */
export const BACKFILL_PARTIAL: readonly (PartialBackfill & { platform: string })[] =
  BACKFILL_LIST_ONLY_PLATFORMS
    .map((id) => {
      const plan = backfillPlanFor(id)!;
      return plan.partial ? { platform: id, ...plan.partial } : null;
    })
    .filter((x): x is PartialBackfill & { platform: string } => x !== null);

// ---------------------------------------------------------------------------
// back-compat：ChatGPT 的两个 URL 构造器。既有测试与接线仍在用。
// ---------------------------------------------------------------------------

export function listPageUrl(origin: string, offset: number, limit = DEFAULT_LIST_LIMIT): string {
  return CHATGPT_PLAN.listUrl(origin, offset, limit);
}

export function detailUrl(origin: string, conversationId: string): string {
  return chatgptDetailUrl(origin, conversationId);
}
