import { INBOX_PREFIX } from './contract';

export interface PublishResult {
  finalName: string;
  bytes: number;
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
export async function writeCommitted(destSlug: string, dataStr: string): Promise<PublishResult> {
  const finalName = `${INBOX_PREFIX}/${destSlug}.json`;
  const partName = `${finalName}.part`;
  const dataUrl = `data:application/json;charset=utf-8,${encodeURIComponent(dataStr)}`;

  const bytes = new TextEncoder().encode(dataStr).length;

  let partId: number | undefined;
  try {
    partId = await browser.downloads.download({
      url: dataUrl,
      filename: partName,
      conflictAction: 'overwrite',
      saveAs: false,
    });
    await waitComplete(partId);
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
    await waitComplete(finalId);

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

const pending = new Map<number, { resolve: () => void; reject: (e: Error) => void; timer: ReturnType<typeof setTimeout> }>();

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
    if (delta.state?.current === 'complete') {
      clearTimeout(entry.timer);
      pending.delete(delta.id);
      entry.resolve();
    } else if (delta.state?.current === 'interrupted') {
      clearTimeout(entry.timer);
      pending.delete(delta.id);
      entry.reject(new Error(`download ${delta.id} interrupted`));
    }
  });
}

function waitComplete(id: number, timeoutMs = 15_000): Promise<void> {
  ensureChangeListener();
  return new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`download ${id} timed out`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
  });
}