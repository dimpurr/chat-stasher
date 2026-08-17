import { INBOX_PREFIX } from './contract';
import { classifyDelta, stalledResult, DOWNLOAD_TIMEOUT_MS, type DownloadResult } from './download-guard';

export interface PublishResult {
  finalName: string;
  bytes: number;
}

export interface WriteOptions {
  /** 一次下载的观测窗口。到点还没终态就判为「停滞」。 */
  timeoutMs?: number;
  /**
   * 每一次下载观测到终态（或判定停滞）都回调一次。
   * C12：这是守卫唯一的数据来源；失败不阻断落盘路径，回调抛错也不许打断写入。
   */
  onOutcome?: (result: DownloadResult) => void | Promise<void>;
}

/** 携带三态结论的下载错误，让调用方能区分「明确失败」和「停滞」。 */
export class DownloadFailure extends Error {
  constructor(readonly result: DownloadResult, message: string) {
    super(message);
    this.name = 'DownloadFailure';
  }
}

/**
 * Zero-config delivery channel: chrome.downloads.
 * WHY downloads over File System Access / Native Messaging:
 *  - FSA picker cannot be opened from a service worker (no DOM window),
 *  - Native Messaging is explicitly deferred to phase 2 (store pain),
 *  - downloads needs only the "downloads" permission.
 *
 * ATOMICITY — promised behaviour vs reality:
 *  - Chrome cannot RENAME files on disk from an extension (downloads has no
 *    rename API; the SW has no FS access). True "temp name → rename" is NOT
 *    possible on this channel. (See `removeFile` below for why delete IS.)
 *  - What downloads DOES give us: a file is committed to disk only after the
 *    whole blob has been received; for data: URLs the blob is fully materialised
 *    in memory before any disk write starts, so the write window is one bounded
 *    flush — no streaming partial content is ever visible for any length of time.
 *  - We still keep the ".part" ritual: the payload is written first under the
 *    final name + ".part". Only when that complete event fires is the identical
 *    payload re-issued under the final name. A CLI picking the inbox ONLY reads
 *    files ending in ".json" and MUST ignore "*.part". If a worker crashes
 *    mid-sequence only a ".part" file exists => never a half final file.
 *  - Fixed vs C1: the leftover ".part" IS now removed from disk once the final
 *    file is confirmed complete, via downloads.removeFile(partId) — an API that
 *    exists on both Chromium and Firefox (extension may only touch downloads it
 *    initiated itself, which is our case). After a successful sequence the inbox
 *    contains exactly one file (the final .json). Residual, honest: had the
 *    part already gone (user removed it) or the OS refuses to unlink (file open
 *    elsewhere), removeFile rejects and we log the reason — a stale ".part"
 *    may remain, and the CLI should still GC stale "*.part" for that case.
 */

/**
 * Remove the on-disk file of a download we initiated. Extensions may only
 * delete files from their own downloads (that is our case). Rejects when the
 * file cannot be unlinked; the caller decides whether that is fatal.
 */
export async function removeDownloadedFile(id: number, what: string): Promise<void> {
  const api = (browser.downloads as unknown) as { removeFile?: (d: number) => Promise<void> };
  if (typeof api.removeFile !== 'function') {
    throw new Error(`removeFile unavailable (cannot remove ${what})`);
  }
  await api.removeFile(id);
}

/** Hide a completed download from the shelf. Cosmetic; failure is non-fatal. */
async function eraseFromShelf(id: number): Promise<void> {
  try {
    await browser.downloads.erase({ id });
  } catch { /* cosmetic only */ }
}

/**
 * Two-phase write whose END STATE is a single final file (no ".part" on disk).
 * 1. write `<final>.json.part`, wait for its complete event;
 * 2. write `<final>.json`, wait for its complete event;
 * 3. removeFile(partId) — delete the tail (".part") from the disk;
 * 4. erase(partId) — tidy the Downloads shelf.
 * On any failure the ".part" from THIS call is best-effort removed too, so we
 * never deliberately leave a ".part" behind; only an unlink refusal does.
 */
export async function writeCommitted(
  destSlug: string,
  dataStr: string,
  options: WriteOptions = {},
): Promise<PublishResult> {
  const finalName = `${INBOX_PREFIX}/${destSlug}.json`;
  const partName = `${finalName}.part`;
  const dataUrl = `data:application/json;charset=utf-8,${encodeURIComponent(dataStr)}`;

  const bytes = new TextEncoder().encode(dataStr).length;
  const timeoutMs = options.timeoutMs ?? DOWNLOAD_TIMEOUT_MS;
  const observe = async (id: number) => {
    const result = await waitTerminal(id, timeoutMs);
    // 观测通报绝不允许把落盘路径带下水。
    try {
      await options.onOutcome?.(result);
    } catch (err) {
      console.warn('[chat-stasher] download outcome hook failed', (err as Error).message);
    }
    if (result.outcome !== 'complete') {
      throw new DownloadFailure(
        result,
        result.outcome === 'stalled'
          ? `download ${id} did not reach a terminal state within ${timeoutMs}ms`
          : `download ${id} interrupted (${result.error ?? 'no reason reported'})`,
      );
    }
  };

  let partId: number | undefined;
  try {
    partId = await browser.downloads.download({
      url: dataUrl,
      filename: partName,
      conflictAction: 'overwrite',
      saveAs: false,
    });
    await observe(partId);
  } catch (err) {
    if (partId !== undefined) await bestEffortRemove(partId, partName);
    throw err;
  }

  try {
    const finalId = await browser.downloads.download({
      url: dataUrl,
      filename: finalName,
      conflictAction: 'overwrite',
      saveAs: false,
    });
    await observe(finalId);

    await bestEffortRemove(partId, partName);
    await eraseFromShelf(partId);

    return { finalName, bytes };
  } catch (err) {
    if (partId !== undefined) await bestEffortRemove(partId, partName);
    throw err;
  }
}

/** removeFile but never throws into the save path — log the reason instead. */
async function bestEffortRemove(id: number, what: string): Promise<void> {
  try {
    await removeDownloadedFile(id, what);
  } catch (err) {
    // Honest non-fatal warning: the final file is correct; only the ".part"
    // cleanup failed (file locked / browser quirk). CLI GC covers the rest.
    console.warn('[chat-stasher] could not delete', what, (err as Error).message);
  }
}

const pending = new Map<number, { settle: (r: DownloadResult) => void; timer: ReturnType<typeof setTimeout> }>();

let changeListenerRegistered = false;

/**
 * The completion listener must not run at module import time: WXT imports
 * entrypoints at build time for discovery, and a top-level browser API call
 * there crashes the build (and SW restarts re-import lazily anyway). Register
 * once, on first use.
 */
function ensureChangeListener(): void {
  if (changeListenerRegistered) return;
  changeListenerRegistered = true;
  browser.downloads.onChanged.addListener((delta) => {
    const entry = pending.get(delta.id);
    if (!entry) return;
    const terminal = classifyDelta(delta);
    // 非终态事件（filename/paused/bytesReceived …）一律不作数：只有真终态才收摊。
    if (!terminal) return;
    clearTimeout(entry.timer);
    pending.delete(delta.id);
    entry.settle({ ...terminal, waitedMs: 0 });
  });
}

/**
 * 观测一次下载直到终态或窗口用尽。
 * 🔴 三态都以「结果」返回，绝不把停滞和失败混成同一个错误 —— 处置不同：
 *    失败可重试，停滞重试只会再制造一次同样的打扰。
 */
function waitTerminal(id: number, timeoutMs: number): Promise<DownloadResult> {
  ensureChangeListener();
  const startedAt = Date.now();
  return new Promise<DownloadResult>((resolve) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      resolve(stalledResult(timeoutMs));
    }, timeoutMs);
    pending.set(id, {
      settle: (r) => resolve({ ...r, waitedMs: Date.now() - startedAt }),
      timer,
    });
  });
}