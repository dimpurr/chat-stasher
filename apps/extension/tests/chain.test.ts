import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  mkdtempSync, writeFileSync, readFileSync, statSync, readdirSync,
  existsSync, mkdirSync, unlinkSync,
} from 'node:fs';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { CAPTURE_EVENT, type CapturedFetch } from '../lib/contract';

/**
 * End-to-end synthetic chain test (no real DeepSeek login):
 *   MAIN fetch hook (world MAIN) -> CustomEvent -> ISOLATED bridge
 *   -> runtime.sendMessage -> background -> chrome.downloads -> real file.
 *
 * The real entrypoint sources are loaded and executed; the browser API is a
 * minimal mock whose downloads implementation writes actual bytes to a temp
 * "Downloads" directory so the 落盘 step is verifiable on real filesystem.
 */

interface FakeDownloadRecord {
  id: number;
  filename: string;
  bytes: number;
}

let downloadsRoot = /* per-test temp dir, see beforeEach */ '';

const runtimeListeners: Array<{
  fn: (msg: any, sender: any, sendResponse: (r: any) => void) => any;
}> = [];

const downloadRecords: FakeDownloadRecord[] = [];
let nextId = 1;
const onChangedListeners: Array<(d: any) => void> = [];

function emitDownloadState(id: number, state: string) {
  for (const l of onChangedListeners) l({ id, state: { previous: 'in_progress', current: state } });
}

/** Each test simulates a fresh extension: clear all registered listeners/records. */
function resetMocks() {
  runtimeListeners.length = 0;
  downloadRecords.length = 0;
  nextId = 1;
  vi.unstubAllGlobals();
  // NOTE: onChangedListeners is deliberately NOT cleared. download.ts registers
  // its change listener once per module (module-level flag + module cache), so
  // wiping the array here would leave later tests with a registered flag but an
  // empty listener list -> completion events never feed waitComplete -> hangs.
}

beforeEach(() => {
  // Fresh fake "Downloads" home per test: capture results must never bleed
  // from one test into another (evidence stays truthful per scenario).
  downloadsRoot = mkdtempSync(join(tmpdir(), 'chat-stasher-mock-downloads-'));
  resetMocks();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineContentScript', (cfg: any) => cfg);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: {
      addListener() { /* Q2 refresh hook is a no-op in the FS-channel test */ },
    },
    onMessage: {
      addListener(fn: any) {
        runtimeListeners.push({ fn });
      },
    },
    async sendMessage(msg: any): Promise<any> {
      return new Promise((resolve, reject) => {
        let settled = false;
        const doResolve = (v: any) => {
          if (!settled) {
            settled = true;
            resolve(v);
          }
        };
        for (const { fn } of runtimeListeners) {
          try {
            const ret = fn(msg, { id: 'mock-sender', url: null }, doResolve);
            if (ret && typeof ret.then === 'function') {
              ret.then(doResolve).catch(reject);
            } else if (ret !== true) {
              // sync listener without sendResponse contract
              doResolve(ret);
            }
            // ret === true => async, waits for sendResponse() (called above)
          } catch (err) {
            reject(err);
            return;
          }
        }
        setTimeout(() => doResolve(undefined), 5000).unref?.();
      });
    },
  },
  downloads: {
    onChanged: {
      addListener(fn: any) {
        onChangedListeners.push(fn);
      },
    },
    async download(opts: { url: string; filename: string }): Promise<number> {
      // Decode data: URL payload and write real bytes into the fake Downloads dir.
      const m = /^data:application\/json[^,]*,/.exec(opts.url);
      if (!m) throw new Error('mock only supports data:application/json URLs');
      const text = decodeURIComponent(opts.url.slice(m[0].length));
      const abs = join(downloadsRoot, opts.filename);
      // Chrome auto-creates subdirectories for downloads; mirror that.
      mkdirSync(dirname(abs), { recursive: true });
      // writeFileSync is a full single write; a complete mock "commit".
      writeFileSync(abs, text, 'utf8');
      const id = nextId++;
      downloadRecords.push({ id, filename: opts.filename, bytes: Buffer.byteLength(text, 'utf8') });
      setTimeout(() => emitDownloadState(id, 'complete'), 0);
      return id;
    },
    /**
     * Mirror chrome.downloads.removeFile: physical unlink of OUR OWN download.
     * This is what C2 relies on to leave exactly one final file on disk.
     */
    async removeFile(id: number): Promise<void> {
      const rec = downloadRecords.find((d) => d.id === id);
      if (!rec) throw new Error(`removeFile: no such download ${id}`);
      const abs = join(downloadsRoot, rec.filename);
      try {
        unlinkSync(abs);
      } catch {
        // Already gone / locked — chrome rejects too; caller treats as non-fatal.
        throw new Error(`removeFile: cannot unlink ${rec.filename}`);
      }
    },
    async erase(_q: { id?: number }): Promise<boolean> {
      return true;
    },
  },
};

async function loadBackground() {
  const mod = await import('../entrypoints/background');
  const bg = (mod as any).default;
  await bg(); // simulate service worker startup
}

async function loadBridge() {
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

async function loadMainHook(window: any) {
  const mod: any = await import('../entrypoints/dw-fetch-main.content');
  mod.default.main();
  return mod.default;
}

function makeFakeWindow() {
  const listeners: Record<string, Array<(e: any) => void>> = {};
  return {
    listeners,
    addEventListener(name: string, fn: (e: any) => void) {
      (listeners[name] ??= []).push(fn);
    },
    dispatchEvent(e: any) {
      for (const fn of listeners[e.type] ?? []) fn(e);
      return true;
    },
    fetch: null as any,
  };
}

describe('chat-stasher capture chain', () => {
  it('MAIN hook -> CustomEvent -> bridge -> background -> real file on disk', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    const rawBody = JSON.stringify({
      session_id: 'c622b5dd-0000-4000-8000-00000000abcd',
      message: { content: 'synthetic answer, never printed in reports', reasoning_content: '' },
    });
    // The "page" original fetch returns a synthetic DeepSeek session response.
    fakeWin.fetch = async (url: string) => {
      return new Response(rawBody, { status: 200, headers: { 'content-type': 'application/json' } });
    };
    await loadMainHook(fakeWin);

    // Fire a fake page call hitting the chat/session endpoint.
    const fakeUrl = 'https://chat.deepseek.com/api/v0/chat/session/c622b5dd-0000-4000-8000-00000000abcd';
    await (fakeWin.fetch as any)(fakeUrl);

    // Allow download completion events + microtasks to flush.
    await new Promise((r) => setTimeout(r, 100));

    expect(downloadRecords.length).toBeGreaterThanOrEqual(1);

    const final = downloadRecords.find((d) => d.filename.endsWith('.json'));
    expect(final).toBeDefined();
    const part = downloadRecords.find((d) => d.filename.endsWith('.json.part'));
    // Two-phase ritual still happened: a ".part" download WAS issued.
    expect(part).toBeDefined();

    const finalAbs = join(downloadsRoot, final!.filename);
    expect(existsSync(finalAbs)).toBe(true);
    const stat = statSync(finalAbs);
    const content = readFileSync(finalAbs, 'utf8');
    const doc = JSON.parse(content);
    expect(doc.schema).toBe('chat-stasher/inbox@2');
    expect(doc.platform).toBe('deepseek');
    expect(doc.sessionId).toBe('c622b5dd-0000-4000-8000-00000000abcd');
    // C3: this synthetic body carries no account fields, so the ADR-002 chain
    // falls through to an explicit 'default' marker — never silently missing.
    expect(doc.identity).toEqual({ level: 'default', value: '' });
    // raw.bytes = size of the ORIGINAL response; final.bytes = whole bundle.
    expect(doc.raw.bytes).toBe(Buffer.byteLength(rawBody, 'utf8'));
    expect(final!.bytes).toBeGreaterThan(doc.raw.bytes);

    // Q1 regression: after a successful capture the on-disk inbox holds exactly
    // ONE file (the final .json) — the ".part" must have been physically
    // removed by downloads.removeFile during writeCommitted.
    const inboxAbs = join(downloadsRoot, 'chat-stasher', 'inbox');
    const onDisk = readdirSync(inboxAbs).sort();
    expect(onDisk).toEqual([final!.filename.split('/').pop()]);
    expect(onDisk.some((f) => f.endsWith('.part'))).toBe(false);
    // Disk size must equal exactly what we serialised (bundle grows whenever the
    // contract does, so compare against content rather than a magic number).
    expect(statSync(join(inboxAbs, onDisk[0]!)).size).toBe(Buffer.byteLength(content, 'utf8'));

    // Evidence block (filenames + byte counts only, never content).
    const ls = onDisk
      .map((f) => `${statSync(join(inboxAbs, f)).size}  ${f}`)
      .join('\n');
    console.log('C2-EVIDENCE downloads root:', downloadsRoot);
    console.log('C2-EVIDENCE ls -la inbox after capture:\n' + ls);
    console.log('C2-EVIDENCE json bytes:', final!.bytes);
    console.log('C2-EVIDENCE part bytes:', part!.bytes);
    void content;
  });

  it('ignores non-session traffic (GET, other origin, non-chat path)', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    fakeWin.fetch = async () => new Response('{}', { status: 200 });
    await loadMainHook(fakeWin);

    const nonSessionUrls = [
      'https://chat.deepseek.com/api/v0/users/me',
      'https://chat.deepseek.com/api/v0/payments/session/sess-x',
      'https://example.com/api/v0/chat/anything', // not DeepSeek origin
    ];
    for (const u of nonSessionUrls) await (fakeWin.fetch as any)(u);
    await new Promise((r) => setTimeout(r, 300));

    expect(downloadRecords.length).toBe(0);
    console.log('C2-EVIDENCE non-session traffic produced zero files:', downloadRecords.length);
  });

  it('[Q1 regression] one capture leaves exactly one inbox file, no .part', async () => {
    // Drive the background's own save contract directly (same entry point the
    // message bus listener calls). The full MAIN->bridge->bus walk is already
    // covered by the first test; this one is focused on the save-channel end
    // state so it cannot race against earlier tests' re-registered listeners.
    const { handleCaptured } = await import('../entrypoints/background');

    const payload: CapturedFetch = {
      url: 'https://chat.deepseek.com/api/v0/chat/session/aaaa1111-bbbb-4000-8000-00000000ffff',
      method: 'POST',
      status: 200,
      text: JSON.stringify({ session_id: 'aaaa1111-bbbb-4000-8000-00000000ffff', message: { content: 'x' } }),
      capturedAt: Date.now(),
    };
    await handleCaptured(payload);

    const inboxAbs = join(downloadsRoot, 'chat-stasher', 'inbox');
    expect(existsSync(inboxAbs)).toBe(true);
    const onDisk = readdirSync(inboxAbs).sort();
    expect(onDisk).toEqual(['deepseek-aaaa1111-bbbb-4000-8000-00000000ffff.json']);
    console.log(
      '[Q1] after one capture inbox files:',
      onDisk.join(', ') || '(empty)',
    );
  });
});