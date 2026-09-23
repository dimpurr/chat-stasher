/**
 * W69 · **Live-capture health is measurable, and an identity change is not a
 * failure.**
 *
 * ## The two defects this file exists for
 *
 * 1. **Nothing recorded when a live capture last arrived, per platform.** The
 *    W68 measurement of the ChatGPT / Grok / Kimi pages could say a great deal
 *    about the hook's *identity* and nothing at all about whether conversations
 *    were still being archived — because the only per-delivery store,
 *    `cs_last_delivered_v1` (lib/recapture.ts), maps a delivery name to a sha256
 *    and carries **no time field**. "Did a live capture arrive at time T" was a
 *    gap in the record, not a negative result. The first group below is that gap
 *    closed, and it is closed at the one place a live capture is decided to be
 *    stored (`handleCaptured`, `entrypoints/background.ts`), through the real
 *    background entry point.
 *
 * 2. **An identity change was reported to the user as "capture is broken".** On
 *    chatgpt.com, grok.com and www.kimi.com the wrapper that replaced ours was
 *    measured to *forward* to the value it captured — which was ours — so
 *    `hook-was-replaced` is an identity-change observation and not a measurement
 *    of lost traffic. The popup's sentence nevertheless read "its live-capture
 *    hook is not working", so the user was told a page was not archiving while
 *    the evidence said only that something wrapped the transport. All three
 *    reasons live in one closed vocabulary (`lib/contract.ts`) and must stay
 *    three different facts.
 *
 * ## What the popup must say instead
 *
 * What is *known*: the observation (with its reason code and its time) and when
 * a live capture from that platform was last stored. The verdict follows from
 * those two facts and nothing else:
 *
 *   · a stored capture at or after the observation ⇒ **working**. A stored
 *     conversation is a measurement of the whole path (page → bridge →
 *     background → host → archive); no inference about a wrapper's identity
 *     outranks a measurement. It is scoped to the platform, because that is
 *     what the record is kept per: the capture may have come from another tab
 *     of the same platform, so the sentence must not claim *this* document
 *     delivered it.
 *   · otherwise, an observation that says our hook is not in effect on that
 *     page (`hook-did-not-run`, `hook-did-not-take`) ⇒ **not working**. Those
 *     two say the page's transport is not ours; nothing is there to capture, and
 *     nothing has been stored since.
 *   · otherwise (an identity change, and no capture on record since it) ⇒
 *     **unknown**. Never "working" and never "broken": whether the wrapper that
 *     took the global still calls through to the one it replaced is exactly what
 *     was not determined.
 *
 * ## How this file reads those facts
 *
 * Every note assertion runs through `renderPopup`, the popup's pure render layer,
 * against the **real** `locales/en.yml` (tests/i18n-harness.ts compiles it with
 * the package's own compiler), so a sentence asserted here is the sentence that
 * ships — the same way tests/w36c-refusal-visible.test.ts and
 * tests/w30-popup-summary.test.ts assert the copy they care about. The storage
 * group runs through the real `runtime.onMessage` listener of
 * `entrypoints/background`, with only `browser.*` and the native host swapped for
 * stubs, the same shape as tests/w3-recapture.test.ts.
 *
 * 🔴 The storage key is written as a **literal** in the key-pinning case on
 *    purpose: it is the name `docs/privacy.md` publishes, and a test that asked
 *    the module for it would pass after a rename that silently stranded the row
 *    under a name the documentation no longer describes.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import {
  HOOK_REASON_DID_NOT_RUN,
  HOOK_REASON_DID_NOT_TAKE,
  HOOK_REASON_WAS_REPLACED,
  type HookObservation,
} from '../lib/contract';
import { hookStatusKey, type HookStatusRecord } from '../lib/hook-status';
import { NO_FAILURES, renderPopup, type PopupModel } from '../lib/popup-view';
import { stamp } from '../lib/ui-strings';

const ORIGIN = 'https://chatgpt.com';
const PLATFORM = 'chatgpt';
const OTHER_PLATFORM = 'deepseek';

/** The row `docs/privacy.md` publishes for this record. Spelled out, not imported. */
const LIVE_KEY = `cs_live_capture_v1:${PLATFORM}`;

const CHATGPT_SID = 'c0ffee00-1111-4222-8333-9a0b0c0d0e0f';
const DEEPSEEK_SID = 'd0d0d0d0-1111-4222-8333-9a0b0c0d0e0f';

/** One observation's moment, and captures that are unambiguously either side of it. */
const OBSERVED_AT = 1_700_000_000_000;
const CAPTURE_AFTER = OBSERVED_AT + 60_000;
const CAPTURE_BEFORE = OBSERVED_AT - 60_000;

// ---------------------------------------------------------------------------
// The fake extension surface (the pattern of tests/w3-recapture.test.ts)
// ---------------------------------------------------------------------------

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];

/** 'up' acks every delivery; 'down' makes every host call throw. */
let hostMode: 'up' | 'down' = 'up';
let lastCapturedAt = 1_700_000_000_000;

const fakeBrowser: any = {
  runtime: {
    id: 'w69-live-capture-health',
    onStartup: { addListener() { /* the startup badge refresh is not under test */ } },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    async sendNativeMessage(_host: string, message: Record<string, any>) {
      if (hostMode === 'down') throw new Error('Specified native messaging host not found.');
      if (message.type === 'hello') {
        return { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: '/stage' };
      }
      if (message.type === 'deliver') {
        return {
          protocol: 1, type: 'ack', request_id: message.request_id,
          status: 'stored', sha256: message.sha256, shard: `0001-${message.name}`,
        };
      }
      throw new Error(`unexpected ${message.type}`);
    },
  },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: {
    badgeText: '' as string,
    async setBadgeText(o: { text: string }) { fakeBrowser.action.badgeText = o.text; },
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

/** A ChatGPT response of the shape the platform table declares (synthetic throughout). */
function chatgptBody(sid: string, answer = 'synthetic answer text'): string {
  return JSON.stringify({
    conversation_id: sid,
    current_node: 'node-1',
    safe_urls: [],
    mapping: {
      'node-0': { id: 'node-0', message: { content: { parts: ['synthetic question'] } } },
      'node-1': { id: 'node-1', message: { content: { parts: [answer] } } },
    },
    account_id: 'acct-synthetic',
  });
}

function chatgptCapture(text: string, sid = CHATGPT_SID): CapturedFetch {
  return {
    url: `https://chatgpt.com/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text,
    pageUrl: `https://chatgpt.com/c/${sid}`,
    capturedAt: (lastCapturedAt += 1),
  };
}

/** A second platform, so "one row per platform" is asserted rather than assumed. */
function deepseekCapture(sid = DEEPSEEK_SID): CapturedFetch {
  return {
    url: `https://chat.deepseek.com/api/v0/chat/session/${sid}`,
    method: 'POST',
    status: 200,
    text: JSON.stringify({ session_id: sid, message: { content: 'synthetic' } }),
    capturedAt: (lastCapturedAt += 1),
  };
}

/** Run one capture through the real onMessage listener and await its answer. */
async function dispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  const replied = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return replied;
}

/** The live-capture row as it sits in storage, or undefined when none was written. */
function liveRow(): any {
  return store[LIVE_KEY];
}

beforeEach(() => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  vi.resetModules();
  (globalThis as any).indexedDB = new IDBFactory();
  hostMode = 'up';
  lastCapturedAt = 1_700_000_000_000;
  fakeBrowser.action.badgeText = '';
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

// ---------------------------------------------------------------------------
// 1 · the popup's pure model, over the real catalog
// ---------------------------------------------------------------------------

/** One origin's hook record, at one moment. */
function record(
  reason: HookObservation = HOOK_REASON_WAS_REPLACED,
  at: number = OBSERVED_AT,
): HookStatusRecord {
  return { origin: ORIGIN, platform: PLATFORM, reasons: [{ reason, at }], at };
}

function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    ...overrides,
  };
}

/** What the popup says about one origin, with the capture evidence it was handed. */
function note(
  hookRecord: HookStatusRecord,
  liveCapture?: Array<{ platform: string; at: number; count: number }>,
): string {
  return renderPopup(model({ hookStatus: [hookRecord], liveCapture })).notes.join('\n');
}

/** A stored live capture for one platform. */
function capture(at: number, platform: string = PLATFORM, count = 1) {
  return { platform, at, count };
}

// ---------------------------------------------------------------------------
// 1 · the record exists at all
// ---------------------------------------------------------------------------

describe('W69 · a stored live capture is written down, per platform', () => {
  it('🔴 writes the row when a capture from that platform is stored', async () => {
    const reply = await dispatch(chatgptCapture(chatgptBody(CHATGPT_SID)));

    // The capture really was stored — otherwise this test would be asserting a
    // record about something that did not happen.
    expect(reply.saved).toBe(true);
    expect(reply.status).toBe('delivered');

    const row = liveRow();
    expect(row, 'no live-capture row was written for a stored capture').toBeDefined();
    expect(row.platform).toBe(PLATFORM);
    expect(typeof row.at).toBe('number');
    expect(Number.isFinite(row.at)).toBe(true);
    expect(row.count).toBe(1);
  });

  it('counts each stored capture and keeps the newest time', async () => {
    await dispatch(chatgptCapture(chatgptBody(CHATGPT_SID)));
    const first = liveRow();
    await dispatch(chatgptCapture(chatgptBody(CHATGPT_SID, 'synthetic answer, edited')));
    const second = liveRow();

    expect(second.count).toBe(2);
    expect(second.at).toBeGreaterThanOrEqual(first.at);
  });

  it('keeps one row per platform, and says nothing about platforms that delivered nothing', async () => {
    await dispatch(chatgptCapture(chatgptBody(CHATGPT_SID)));
    expect(await dispatch(deepseekCapture())).toMatchObject({ saved: true, status: 'delivered' });

    expect(liveRow().platform).toBe(PLATFORM);
    const other = store[`cs_live_capture_v1:${OTHER_PLATFORM}`] as { platform: string; count: number };
    expect(other.platform).toBe(OTHER_PLATFORM);
    expect(other.count).toBe(1);
    // 🔴 A platform with no delivery has **no row**, which is the honest record of
    //    "we have nothing on this platform" — not a row holding zero.
    expect(store['cs_live_capture_v1:kimi']).toBeUndefined();
  });

  it('🔴 a capture that was NOT stored is not recorded as an arrival', async () => {
    hostMode = 'down';
    const reply = await dispatch(chatgptCapture(chatgptBody(CHATGPT_SID)));

    expect(reply.saved).toBe(false);
    expect(reply.status).toBe('queued');
    // Queued is a separate outcome and not an arrival: recording it would make
    // "a conversation reached the archive" true of a conversation that did not.
    expect(liveRow()).toBeUndefined();
  });

  it('a capture the live leg refused is not recorded as an arrival', async () => {
    // No usable conversation identity in the URL or the body ⇒ `preparePayload`
    // refuses it, so the live leg really does answer "not stored".
    const reply = await dispatch(
      chatgptCapture(JSON.stringify({ mapping: {}, current_node: 'node-1' }), 'not-a-uuid'),
    );

    expect(reply.saved).toBe(false);
    expect(reply.status).toBe('refused');
    expect(liveRow()).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 2 · the record's reader
// ---------------------------------------------------------------------------

describe('W69 · the record is read back strictly', () => {
  it('a value under the prefix that is not a record is left out, not repaired', async () => {
    const { liveCaptureOf, liveCaptureKey } = await import('../lib/live-capture');
    const snapshot = {
      [liveCaptureKey(PLATFORM)]: { platform: PLATFORM },
      [`${liveCaptureKey(PLATFORM)}_foreign`]: { platform: OTHER_PLATFORM, at: 5, count: 1 },
      [hookStatusKey(ORIGIN)]: undefined,
    };

    // A row that contradicts itself, or that is not a row at all, is not read as
    // one: the same rule `hookStatusOf` follows.
    expect(liveCaptureOf(snapshot)).toEqual([]);
    expect(liveCaptureOf(null)).toEqual([]);
  });

  it('reads a real row back, newest first, and drops one whose key disagrees with it', async () => {
    const { liveCaptureOf, liveCaptureKey } = await import('../lib/live-capture');
    const older = { platform: PLATFORM, at: 10, count: 1 };
    const newer = { platform: OTHER_PLATFORM, at: 20, count: 2 };
    const snapshot = {
      [liveCaptureKey(PLATFORM)]: older,
      [liveCaptureKey(OTHER_PLATFORM)]: newer,
      ['cs_live_capture_v1:someone-else']: older,
    };

    expect(liveCaptureOf(snapshot).map((r) => r.platform)).toEqual([OTHER_PLATFORM, PLATFORM]);
  });
});

// ---------------------------------------------------------------------------
// 3 · the popup's sentence
// ---------------------------------------------------------------------------

describe('W69 · the popup presents the capture evidence, not just the observation', () => {
  it('🔴 the sentence changes when a live capture is on record for that platform', () => {
    const withCapture = note(record(), [capture(CAPTURE_AFTER, PLATFORM, 3)]);
    const without = note(record());

    expect(withCapture).not.toBe(without);
  });

  it('🔴 says when the last live capture was stored', () => {
    const text = note(record(), [capture(CAPTURE_AFTER, PLATFORM, 3)]);
    expect(text).toContain(stamp(CAPTURE_AFTER));
  });

  it('🔴 an identity change alone is presented as unknown, never as breakage', () => {
    const text = note(record(HOOK_REASON_WAS_REPLACED)).toLowerCase();
    expect(text).toContain('whether capture still works there is unknown');
    expect(text).not.toContain('not being archived');
  });

  it('🔴 a hook that is not in effect, with no capture since, is presented as breakage', () => {
    for (const reason of [HOOK_REASON_DID_NOT_RUN, HOOK_REASON_DID_NOT_TAKE] as const) {
      const text = note(record(reason)).toLowerCase();
      expect(text, `${reason} must be reported as a failure to the user`).toContain('not being archived');
      expect(text).not.toContain('whether capture still works there is unknown');
      expect(text).not.toContain('live capture from this platform is working');
    }
  });

  it('🔴 a stored capture at or after the observation settles it: working', () => {
    const text = note(record(HOOK_REASON_WAS_REPLACED), [capture(CAPTURE_AFTER)]).toLowerCase();
    expect(text).toContain('live capture from this platform is working');
    expect(text).not.toContain('whether capture still works there is unknown');
    expect(text).not.toContain('not being archived');
  });

  it('🔴 a capture OLDER than the observation is not evidence about it', () => {
    const stale = note(record(HOOK_REASON_WAS_REPLACED), [capture(CAPTURE_BEFORE)]);
    const fresh = note(record(HOOK_REASON_WAS_REPLACED), [capture(CAPTURE_AFTER)]);

    expect(stale).not.toBe(fresh);
    expect(stale.toLowerCase()).toContain('whether capture still works there is unknown');
    // And the same precedence the other way: a stale capture does not stop an
    // absent hook from being reported as absent.
    expect(note(record(HOOK_REASON_DID_NOT_RUN), [capture(CAPTURE_BEFORE)]).toLowerCase())
      .toContain('not being archived');
  });

  it('a capture from ANOTHER platform says nothing about this one', () => {
    const other = note(record(HOOK_REASON_WAS_REPLACED), [capture(CAPTURE_AFTER, OTHER_PLATFORM)]);
    const none = note(record(HOOK_REASON_WAS_REPLACED), []);
    expect(other).toBe(none);
  });

  it('a measurement outranks the inference: a capture after a missing hook still reads as working', () => {
    const text = note(record(HOOK_REASON_DID_NOT_RUN), [capture(CAPTURE_AFTER)]).toLowerCase();
    expect(text).toContain('live capture from this platform is working');
    // 🔴 And the observation is still stated — the measurement settles the
    //    verdict, it does not erase what the page reported about itself.
    expect(text).toContain('no copy of the hook ran in that page at all');
  });

  it('🔴 "we have no record" is not worded as "none arrived"', () => {
    const text = note(record()).toLowerCase();
    expect(text).toContain('not one capture from it has ever been recorded here');
    expect(text).toContain('a gap in this record, not a measurement that none arrived');
  });

  it('states the observation, its time, and does not name a cause', () => {
    const joined = note(record()).toLowerCase();
    expect(joined).toContain(ORIGIN);
    expect(joined).toContain(stamp(OBSERVED_AT).toLowerCase());
    for (const guess of [
      'trusted types',
      'content security policy',
      'csp',
      'usual reason',
      'already open when the extension was loaded',
    ]) {
      expect(joined).not.toContain(guess);
    }
    // The reason codes are codes, not sentences: none may reach a reader.
    for (const reason of [
      HOOK_REASON_DID_NOT_RUN,
      HOOK_REASON_DID_NOT_TAKE,
      HOOK_REASON_WAS_REPLACED,
    ] as const) {
      expect(note(record(reason))).not.toContain(reason);
    }
  });

  it('renders a distinct sentence per reason, as before', () => {
    const sentences = ([
      HOOK_REASON_DID_NOT_RUN,
      HOOK_REASON_DID_NOT_TAKE,
      HOOK_REASON_WAS_REPLACED,
    ] as const).map((reason) => note(record(reason)));
    expect(new Set(sentences).size).toBe(3);
  });
});
