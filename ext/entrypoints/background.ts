import {
  extractIdentity,
  extractSessionId,
  SCHEMA,
  sanitizePathSegment,
  DEEPSEEK_ORIGIN,
  type CapturedFetch,
  type InboxBundle,
} from '../lib/contract';
import { writeCommitted } from '../lib/download';
import { recordCapture, refreshBadge } from '../lib/badge';

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
  const sessionId = extractSessionId(captured.url, captured.text) ?? 'unknown';
  return {
    schema: SCHEMA,
    platform: 'deepseek',
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
  const sessionId = extractSessionId(captured.url, captured.text);
  if (cancelledIdLike(sessionId)) {
    // Per-session naming is the inbox contract; a session-less capture has no
    // stable file name and is dropped rather than polluting the inbox.
    return { saved: false, reason: 'no-session-id (skipped in report only)' };
  }

  const bundle = buildBundle(captured);
  const slug = `deepseek-${sanitizePathSegment(bundle.sessionId)}`;
  const { finalName, bytes } = await writeCommitted(slug, JSON.stringify(bundle));
  // Badge is never allowed to take the save down: fire-and-forget, log-only.
  void recordCapture().catch((err) => {
    console.warn('[chat-stasher] badge update failed', (err as Error).message);
  });
  return { saved: true, finalName, bytes };
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

  console.log('[chat-stasher] background ready, captures go to Downloads/', DEEPSEEK_ORIGIN);
});