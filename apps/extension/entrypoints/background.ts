import {
  extractIdentity,
  extractSessionId,
  SCHEMA,
  sanitizePathSegment,
  findPlatformForUrl,
  type CapturedFetch,
  type InboxBundle,
} from '../lib/contract';
import { writeCommitted } from '../lib/download';
import { recordCapture, refreshBadge } from '../lib/badge';
import { browserLocalStore } from '../lib/backfill/store';
import { tickBackfill, type TickResult } from '../lib/backfill/schedule';
import type { HttpPort } from '../lib/backfill/engine';
import { POPUP_STATUS_MESSAGE, type BackfillRuntimeStatus } from '../lib/popup-view';
import {
  guardBadge,
  isGuardTripped,
  loadGuardState,
  recordDownloadOutcome,
  resumeAfterGuard,
} from '../lib/download-guard';

export interface HandledResult {
  saved: boolean;
  reason?: string;
  finalName?: string;
  bytes?: number;
}

/**
 * Build the inbox JSON document.
 * WHY raw-first: the parsed envelope is best-effort; if our field guesses are
 * wrong the CLI can re-derive structure from `raw.text` instead of losing data.
 */
function buildBundle(captured: CapturedFetch): InboxBundle {
  const parsed = { hasJson: false, keys: [] as string[] };
  try {
    const obj = JSON.parse(captured.text);
    if (obj && typeof obj === 'object') {
      parsed.hasJson = true;
      parsed.keys = Object.keys(obj);
    }
  } catch { /* not JSON */ }
  const sessionId = extractSessionId(captured.url, captured.text, captured.pageUrl) ?? 'unknown';
  const platform = findPlatformForUrl(captured.url) ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  return {
    schema: SCHEMA,
    platform: platform?.id ?? 'deepseek',
    sessionId,
    // ADR-002: the dedupe axis is the ACCOUNT. `sessionId` guard keeps a
    // per-session id from ever being mistaken for the stable account id.
    identity: extractIdentity(captured.text, sessionId === 'unknown' ? null : sessionId),
    url: captured.url,
    method: captured.method,
    status: captured.status,
    capturedAt: new Date(captured.capturedAt).toISOString(),
    parsed,
    raw: {
      text: captured.text,
      bytes: new TextEncoder().encode(captured.text).length,
    },
  };
}

export async function handleCaptured(captured: CapturedFetch): Promise<HandledResult> {
  const sessionId = extractSessionId(captured.url, captured.text, captured.pageUrl);
  if (cancelledIdLike(sessionId)) {
    // Per-session naming is the inbox contract; a session-less capture has no
    // stable file name and is dropped rather than polluting the inbox.
    return { saved: false, reason: 'no-session-id (skipped in report only)' };
  }

  const bundle = buildBundle(captured);
  const slug = `${bundle.platform}-${sanitizePathSegment(bundle.sessionId)}`;
  const { finalName, bytes } = await writeCommitted(slug, JSON.stringify(bundle), {
    // C12：实时腿的每一次写入也喂给守卫（观测是共享的），
    // 但【熔断只暂停回溯腿】—— 这里不做任何阻断，用户当前这条对话照存。
    onOutcome: (result) => paintGuard(result),
  });
  // Badge is never allowed to take the save down: fire-and-forget, log-only.
  void recordCapture().catch((err) => {
    console.warn('[chat-stasher] badge update failed', (err as Error).message);
  });
  return { saved: true, finalName, bytes };
}

/**
 * C12：记一次下载观测，并在熔断时把角标换成告警态。
 * 全程 best-effort —— 守卫是保护层，绝不允许它把落盘路径带下水。
 */
async function paintGuard(result: import('../lib/download-guard').DownloadResult): Promise<void> {
  try {
    const state = await recordDownloadOutcome(browserLocalStore(), result, { now: Date.now() });
    if (!state) return;
    const badge = guardBadge(state);
    if (!badge) return;
    const action = (browser as {
      action?: {
        setBadgeText: (o: { text: string }) => Promise<void>;
        setTitle?: (o: { title: string }) => Promise<void>;
      };
    }).action;
    if (!action) return;
    await action.setBadgeText({ text: badge.text });
    await action.setTitle?.({ title: badge.title });
  } catch (err) {
    console.warn('[chat-stasher] download guard update failed', (err as Error).message);
  }
}

/** 回溯腿的闸门：给 runBackfill 的 downloadGuard 用。 */
export async function isBackfillPausedByDownloadGuard(): Promise<boolean> {
  const store = browserLocalStore();
  if (!store) return false;
  return isGuardTripped(await loadGuardState(store));
}

/** 显式恢复入口（不做 UI，函数即接口）。欠账集合不动 ⇒ 从断点继续。 */
export async function resumeBackfillAfterDownloadGuard(): Promise<boolean> {
  const store = browserLocalStore();
  if (!store) return false;
  await resumeAfterGuard(store);
  const action = (browser as { action?: { setBadgeText: (o: { text: string }) => Promise<void> } }).action;
  await action?.setBadgeText({ text: '' });
  return true;
}

// ---------------------------------------------------------------------------
// C13 · 回溯腿的接线
//
// 触发点选的是【实时腿的 onMessage】，不是扩展启动，理由三条：
//  1. MV3 里 SW 平时是死的。真正会把它叫醒的事件本来就只有实时腿这一个 ——
//     选它当心跳，等于零成本地拿到一个"用户正在用某个平台"的信号；
//  2. 目标（platform / origin / 账号 scope）就在这条消息里现成带着。
//     扩展启动时【没有】这些信息（没有当前 tab、没有账号），只能去猜或去存一份，
//     那是凭空多出来的状态；
//  3. 用户正开着那个平台的页面时才慢慢回溯，是最不像爬虫、最温和的时机。
// 🔴 没有用 chrome.alarms 定时：manifest permissions 只有 ['downloads']，
//    没有 'alarms'，而本任务不许新增权限（见 wxt.config.ts:9）。
// ---------------------------------------------------------------------------

/**
 * 🔴 回溯腿的网络端口。**默认 null ⇒ 生产构建里回溯腿绝不会发任何请求。**
 * 想让它真的取数，必须有人显式调用 configureBackfillTransport —— 本仓库里
 * 没有任何生产代码这么做（只有 C13 的测试注入合成端口）。这一条是刻意的：
 * 「会不会用登录态去打平台」必须是一处显式决定，不能藏在默认参数里。
 */
let backfillTransport: HttpPort | null = null;
let lastTick: TickResult | null = null;
let pendingTick: Promise<unknown> = Promise.resolve();

export function configureBackfillTransport(http: HttpPort | null): void {
  backfillTransport = http;
}

/** 最近一次 tick 的结果（给测试/排查用，也给 C18 的 Popup 用）。 */
export function lastBackfillTick(): TickResult | null {
  return lastTick;
}

/**
 * C18 · Popup 问 background 要的运行时事实。全部是「我这边真实是什么」，不含推测。
 * 🔴 transportWired 在生产构建里恒为 false —— configureBackfillTransport
 *    在本仓库里【只被测试调用过】。Popup 拿这个值决定要不要说「缺取数通道」。
 */
export function backfillRuntimeStatus(): BackfillRuntimeStatus {
  return {
    transportWired: backfillTransport !== null,
    lastTickReason: lastTick?.reason ?? null,
  };
}

/** 等待 fire-and-forget 的那次 tick 结束。回溯腿绝不允许拖慢落盘，所以只能这样等。 */
export function backfillTickSettled(): Promise<unknown> {
  return pendingTick;
}

/** 从一次真实捕获里推出回溯目标。推不出来就返回 null（不猜）。 */
export function backfillTargetFor(
  captured: CapturedFetch,
): { platform: string; origin: string; scope: string } | null {
  const row = findPlatformForUrl(captured.url)
    ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  if (!row) return null;
  let origin: string | null = null;
  for (const candidate of [captured.pageUrl, captured.url]) {
    if (!candidate) continue;
    try {
      const o = new URL(candidate).origin;
      if (row.origins.includes(o)) { origin = o; break; }
    } catch { /* 不是合法 URL ⇒ 换下一个候选 */ }
  }
  if (!origin) return null;
  // 归档范围键走 ADR-002 的账号轴；认不出账号就用 'default'（与落盘那边一致）。
  const identity = extractIdentity(captured.text, extractSessionId(captured.url, captured.text, captured.pageUrl));
  return { platform: row.id, origin, scope: identity.value || 'default' };
}

/**
 * 实时腿存完一条之后顺带踢一脚回溯腿。全程 best-effort：
 * 任何异常只进日志，绝不影响用户当前这条对话的落盘。
 */
export async function kickBackfill(captured: CapturedFetch): Promise<TickResult | null> {
  const target = backfillTargetFor(captured);
  if (!target) return null;
  const store = browserLocalStore();
  const result = await tickBackfill({
    ...target,
    store,
    http: backfillTransport ?? undefined,
    downloadGuard: isBackfillPausedByDownloadGuard,
    // 归档出口 = 实时腿同一个落盘函数，逻辑不分叉。
    sink: async (c) => { await handleCaptured(c); },
  });
  lastTick = result;
  return result;
}

function cancelledIdLike(id: string | null): boolean {
  if (!id) return true;
  return id.length < 8 || id === 'unknown';
}

export default defineBackground(async () => {
  browser.runtime.onMessage.addListener(
    (message: { type?: string; payload?: CapturedFetch }, _sender, sendResponse) => {
      // C18：Popup 打开时问一句「取数通道接上没有」。同步答，立刻返回。
      if (message?.type === POPUP_STATUS_MESSAGE) {
        sendResponse(backfillRuntimeStatus());
        return true;
      }
      if (message?.type !== 'chat-captured' || !message.payload) return;
      const payload = message.payload;
      handleCaptured(payload)
        .then((result) => {
          // C13：实时腿这一脚顺带唤醒回溯腿。fire-and-forget —— 回溯是慢活，
          // 绝不允许它挡住 sendResponse 或拖慢用户当前这条对话的落盘。
          pendingTick = kickBackfill(payload).catch((err) => {
            console.warn('[chat-stasher] backfill tick failed', (err as Error).message);
            return null;
          });
          if (result.saved) {
            // Privacy rule: never the conversation content — ids/bytes only.
            console.log(
              `[chat-stasher] saved ${result.bytes} bytes -> ${result.finalName}`,
            );
          }
          sendResponse({ ok: result.saved, ...result });
        })
        .catch((err) => {
          console.error('[chat-stasher] handleCaptured failed', (err as Error).message);
          sendResponse({ ok: false, error: (err as Error).message });
        });
      // MV3: async sendResponse requires returning true to keep the channel open.
      return true;
    },
  );

  // Every SW wake (fresh start AND runtime.onStartup) re-asserts the badge's
  // truth, so a dead-worker leftover badge gets cleared once 5 min pass.
  void refreshBadge();
  browser.runtime.onStartup.addListener(() => {
    void refreshBadge();
  });

  console.log('[chat-stasher] background ready, captures go to Downloads/ for explicit platform origins');
});
