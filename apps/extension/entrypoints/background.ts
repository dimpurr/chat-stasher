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

function cancelledIdLike(id: string | null): boolean {
  if (!id) return true;
  return id.length < 8 || id === 'unknown';
}

export default defineBackground(async () => {
  browser.runtime.onMessage.addListener(
    (message: { type?: string; payload?: CapturedFetch }, _sender, sendResponse) => {
      if (message?.type !== 'chat-captured' || !message.payload) return;
      handleCaptured(message.payload)
        .then((result) => {
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
