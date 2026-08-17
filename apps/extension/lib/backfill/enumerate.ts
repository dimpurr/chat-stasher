/**
 * 枚举段：拿会话列表 + total。
 *
 * 事实与复核状态（诚实标注）：
 *  · 交给我的「已查到的事实」说 ChatGPT 的会话列表是
 *    GET /backend-api/conversations?offset=&limit=，响应里自带 total。
 *    **我没有复核这一条** —— 本任务禁止真调平台接口，我也没有登录态。
 *    所以下面的字段名（items / total）属于**待验证的假设**，不是实测结果。
 *  · 因此这里的解析器写成「形状不合就报 shape-changed 并停」，而不是尽力猜。
 *    假设错了会立刻变成一条留痕的停机记录，不会变成静默爬不动，也不会变成假进度。
 *  · lib/contract.ts:97 已登记的 chatgpt pathHints 是 '/backend-api/conversation/'
 *    （单数，取正文用）；列表是 '/backend-api/conversations'（复数），两者不同。
 */

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

export function listPageUrl(origin: string, offset: number, limit = DEFAULT_LIST_LIMIT): string {
  return `${origin}${CHATGPT_LIST_PATH}?offset=${offset}&limit=${limit}`;
}

export function detailUrl(origin: string, conversationId: string): string {
  return `${origin}${CHATGPT_DETAIL_PATH}${encodeURIComponent(conversationId)}`;
}

/**
 * 解析一页会话列表。
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
