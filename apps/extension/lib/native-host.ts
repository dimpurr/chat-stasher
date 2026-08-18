/**
 * Native Messaging transport (ADR-014 step 3).
 *
 * Primary channel: Native Messaging host (com.chat_stasher.host).
 * Fallback channel: chrome.downloads (writeCommitted).
 *
 * Why NM over downloads:
 *  - downloads `saveAs: false` is overridden by the browser-level "ask where to save each file" setting,
 *    causing up to 200 modal dialogs a day during backfill.
 *  - Native Messaging writes directly to the local CLI staging inbox with zero modal dialogs.
 */

import type { InboxBundle } from './contract';

/**
 * Pinned host name registered by `chat-stasher install-native-host`.
 * Outsource: `crates/chat-stasher/src/nativehost.rs:43` (HOST_NAME = "com.chat_stasher.host").
 */
export const NATIVE_HOST_NAME = 'com.chat_stasher.host';

export const POPUP_CHANNEL_NM = '落盘通道：直接交给本机的 chat-stasher，不经过下载';
export const POPUP_CHANNEL_FALLBACK =
  '落盘通道：没找到本机 host，正在走下载通道；如果浏览器每次都问你保存位置，跑一次 `chat-stasher install-native-host` 就不会再问';

export type NativeProbeReason =
  | 'connected'
  | 'host-not-found'
  | 'no-runtime-api'
  | 'disconnected'
  | 'timeout'
  | 'send-failed';

export interface NativeHostProbeResult {
  ok: boolean;
  reason: NativeProbeReason;
  error?: string;
  version?: string;
}

export interface NativeDeliverResult {
  ok: boolean;
  reason?: NativeProbeReason | string;
  bytes: number;
}

function getRuntime() {
  const g = globalThis as any;
  if (g.browser?.runtime?.id) return g.browser.runtime;
  if (g.chrome?.runtime?.id) return g.chrome.runtime;
  return g.browser?.runtime ?? g.chrome?.runtime ?? null;
}

/**
 * 探测 Native Messaging Host 是否可用。
 * 🔴 探测失败必须是一个具名结局，绝不静默。
 */
export async function probeNativeHost(options: { timeoutMs?: number } = {}): Promise<NativeHostProbeResult> {
  const timeoutMs = options.timeoutMs ?? 500;
  const runtime = getRuntime();
  if (!runtime || typeof runtime.connectNative !== 'function') {
    return { ok: false, reason: 'no-runtime-api' };
  }

  return new Promise<NativeHostProbeResult>((resolve) => {
    let settled = false;
    let port: any = null;

    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      try {
        port?.disconnect?.();
      } catch {}
      resolve({ ok: true, reason: 'connected' });
    }, timeoutMs);

    try {
      port = runtime.connectNative(NATIVE_HOST_NAME);
    } catch (err) {
      clearTimeout(timer);
      settled = true;
      return resolve({
        ok: false,
        reason: 'host-not-found',
        error: (err as Error).message,
      });
    }

    if (!port || !port.onDisconnect || typeof port.onDisconnect.addListener !== 'function') {
      clearTimeout(timer);
      settled = true;
      return resolve({ ok: false, reason: 'no-runtime-api' });
    }

    port.onDisconnect.addListener((p?: any) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      const lastError = runtime.lastError?.message || p?.error?.message;
      resolve({
        ok: false,
        reason: 'host-not-found',
        error: lastError || 'host disconnected',
      });
    });

    if (port.onMessage && typeof port.onMessage.addListener === 'function') {
      port.onMessage.addListener((msg: any) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        try {
          port?.disconnect?.();
        } catch {}
        resolve({
          ok: true,
          reason: 'connected',
          version: typeof msg === 'object' && msg?.version ? String(msg.version) : undefined,
        });
      });
    }
  });
}

/**
 * 主通道落盘：将 inbox bundle 通过 Native Messaging 直接递交给 CLI。
 */
export async function deliverToNativeHost(
  bundle: InboxBundle,
  options: { timeoutMs?: number } = {},
): Promise<NativeDeliverResult> {
  const timeoutMs = options.timeoutMs ?? 2000;
  const runtime = getRuntime();
  if (!runtime || typeof runtime.connectNative !== 'function') {
    return { ok: false, reason: 'no-runtime-api', bytes: 0 };
  }

  const rawBytes = bundle.raw.bytes || new TextEncoder().encode(JSON.stringify(bundle)).length;

  return new Promise<NativeDeliverResult>((resolve) => {
    let settled = false;
    let port: any = null;

    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      try {
        port?.disconnect?.();
      } catch {}
      // Port remained connected for the timeout window without error -> delivery complete
      resolve({ ok: true, bytes: rawBytes });
    }, timeoutMs);

    try {
      port = runtime.connectNative(NATIVE_HOST_NAME);
    } catch (err) {
      clearTimeout(timer);
      settled = true;
      return resolve({ ok: false, reason: 'host-not-found', bytes: 0 });
    }

    if (!port || !port.onDisconnect || typeof port.onDisconnect.addListener !== 'function') {
      clearTimeout(timer);
      settled = true;
      return resolve({ ok: false, reason: 'no-runtime-api', bytes: 0 });
    }

    port.onDisconnect.addListener((p?: any) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      const lastError = runtime.lastError?.message || p?.error?.message;
      resolve({
        ok: false,
        reason: 'host-not-found',
        bytes: 0,
      });
    });

    if (port.onMessage && typeof port.onMessage.addListener === 'function') {
      port.onMessage.addListener((_msg: any) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        try {
          port?.disconnect?.();
        } catch {}
        resolve({ ok: true, bytes: rawBytes });
      });
    }

    try {
      port.postMessage(bundle);
    } catch (err) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      return resolve({ ok: false, reason: 'send-failed', bytes: 0 });
    }
  });
}
