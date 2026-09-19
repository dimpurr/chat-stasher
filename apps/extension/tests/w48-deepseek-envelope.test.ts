/**
 * W48 · **A shape refusal has to say what it saw.**
 *
 * On 2026-09-19 a real logged-in response stopped the DeepSeek backfill leg with
 *
 *     list cursor=first-page (enumerated 0): deepseek list response has no
 *     `data` object (envelope changed?)
 *
 * The halt itself is correct — the parser recognises a reverse-engineered envelope
 * and that response did not carry it — and this file does not weaken it. What was
 * missing is the other half of the sentence: the response is the only thing that
 * could say what the envelope *became*, and it was gone by the time a human read
 * the ledger. Answering "then what was it?" took a second logged-in session, and
 * the next shape change would have cost another one.
 *
 * So a refusal now carries the **structure** of the body it refused — key names,
 * the type of each, array lengths — and nothing else. This file pins four things:
 *
 *   1. the occurrence above now names the keys the response did have (and `data`
 *      present-and-null is told apart from `data` absent);
 *   2. a `biz_data` that is there while `chat_sessions` is not an array names that
 *      level and what is in it instead;
 *   3. the same, on the two body parsers;
 *   4. 🔴 **no value from the response reaches the detail** — not an id, not a
 *      title, not a message body, not a key that is itself content. This is the
 *      assertion that makes the vocabulary a rule rather than a habit.
 *
 * 🔴 What this file does **not** do: it does not establish what DeepSeek's current
 *    envelope is, and it does not make these responses parse. There is no captured
 *    body in this repository and none is invented here — every fixture below is
 *    synthetic, written by hand, and never sent anywhere: the http port is always
 *    injected, no request goes to deepseek.com, and there is no login.
 *
 * 🔴 The control is the point of the last section: a well-formed page and body are
 *    unchanged — same ids, same outcome, same archive — because a refusal that
 *    says more must not also accept more.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_LIST_PATH,
  parseDeepSeekDetailPage,
  parseDeepSeekDetailTree,
  parseDeepSeekListPage,
} from '../lib/backfill/enumerate';
import type { Clock } from '../lib/backfill/pace';
import { stateKey } from '../lib/backfill/types';

const ORIGIN = 'https://chat.deepseek.com';
const LIMIT = 100;

/** The label the marker for a key whose name is not echoed; exported as SHAPE_WITHHELD_KEY. */
const WITHHELD = '<withheld>';

/** Values that are unmistakably conversation-shaped, used by the no-leak section. */
const PRIVATE_ID = 'ds-0007-aaaaaaaa';
const PRIVATE_TITLE = 'Trip to Kyoto planning';
const PRIVATE_TEXT = 'synthetic message body that must never be echoed';
const PRIVATE_TOKEN = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';

const SCOPES = {
  noData: 'acct-w48-no-data',
  nullData: 'acct-w48-null-data',
  notArray: 'acct-w48-not-array',
  control: 'acct-w48-control',
} as const;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock {
  let t = Date.parse('2026-09-19T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

/** One synthetic list row in the reverse-engineered shape. `over` is how a drifted row is built. */
function session(n: number, over: Record<string, unknown> = {}): Record<string, unknown> {
  return { id: `ds-00${String(n).padStart(2, '0')}-aaaaaaaa`, seq_id: 1000 - n, updated_at: 1_755_000_000 + n, ...over };
}

/** One synthetic message in the measured tree shape: a numeric `message_id`, `parent_id` null at a root. */
function msg(messageId: number, parentId: number | null): Record<string, unknown> {
  return { message_id: messageId, parent_id: parentId, role: messageId % 2 === 1 ? 'USER' : 'ASSISTANT' };
}

/** A whole, walkable body — the control's fixture, and the envelope the drifted ones are built from. */
function wholeBody(): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_code: 0,
      biz_msg: 'ok',
      biz_data: { chat_session: { id: 'ds-0001-aaaaaaaa', title: 'synthetic-fixture', current_message_id: 2 }, chat_messages: [msg(1, null), msg(2, 1)] },
    },
  });
}

function backend(pages: string[], body?: string) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === DEEPSEEK_LIST_PATH) {
      const index = Math.min(calls.length - 1, pages.length - 1);
      return { status: 200, text: pages[index] ?? '' };
    }
    if (u.pathname === DEEPSEEK_DETAIL_PATH && body !== undefined) return { status: 200, text: body };
    throw new Error(`unexpected path ${u.pathname}`);
  };
  return { http, calls };
}

async function run(
  store: ReturnType<typeof memoryStore>,
  be: { http: (url: string) => Promise<HttpResponse> },
  scope: string,
  opts: { maxDetails?: number } = {},
) {
  return runBackfill({
    platform: 'deepseek',
    origin: ORIGIN,
    scope,
    store,
    http: be.http,
    clock: fakeClock(),
    listLimit: LIMIT,
    pace: NO_WAIT,
    maxDetails: opts.maxDetails ?? 0,
    sink: (captured) => ({ saved: true, sessionId: captured.sessionId }),
  });
}

// ---------------------------------------------------------------------------
// 1 · 🔴 The occurrence itself, now diagnosable in one pass
// ---------------------------------------------------------------------------
describe('W48-1 · a missing `data` object names what the response did have', () => {
  it('the flattened envelope (biz_data at the top) halts, and the detail carries the top-level keys and their types', async () => {
    const store = memoryStore();
    // The shape a "we moved the envelope up one level" change would produce. Synthetic, hand-written.
    const flattened = JSON.stringify({ code: 0, msg: 'ok', biz_data: { chat_sessions: [session(1)], has_more: false } });
    const be = backend([flattened]);

    const report = await run(store, be, SCOPES.noData);

    // The refusal itself is exactly what it was: same reason, same stop, nothing archived.
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('no `data` object');
    expect(report.newDebts).toBe(0);
    expect(report.state.enumCursor.complete).toBe(false);
    // 🔴 W48 · The half that was missing: every key the response had, sorted, with its
    //    type — and one level into the object that was not `data`, so the renamed
    //    container is visible rather than merely absent.
    expect(report.halted?.detail).toContain(
      '[saw body: {biz_data:{chat_sessions:array(1), has_more:boolean}, code:number, msg:string}]',
    );
    // ...and the trace is persisted, so the sentence above reaches the ledger and survives a restart.
    const persisted = (await store.load(stateKey('deepseek', SCOPES.noData))) as {
      halted?: { reason: string; detail: string };
    };
    expect(persisted.halted?.reason).toBe('shape-changed');
    expect(persisted.halted?.detail).toContain('[saw body: {biz_data:{chat_sessions:array(1), has_more:boolean}, code:number, msg:string}]');
  });

  it('`data` present and null is a third shape, and the detail says so instead of "absent"', () => {
    // The two readings differ by one word on the wire and would be a wild guess apart
    // without this: `data: null` means the container is still named and came back empty,
    // `data` missing means the container was renamed or removed.
    const withNull = parseDeepSeekListPage(JSON.stringify({ code: 0, msg: 'ok', data: null }));
    expect(withNull.ok).toBe(false);
    if (withNull.ok) throw new Error('a null `data` must not parse');
    expect(withNull.detail).toContain('[saw body: {code:number, data:null, msg:string}]');

    const withoutData = parseDeepSeekListPage(JSON.stringify({ code: 0, msg: 'ok' }));
    expect(withoutData.ok).toBe(false);
    if (withoutData.ok) throw new Error('a missing `data` must not parse');
    expect(withoutData.detail).toContain('[saw body: {code:number, msg:string}]');
    // The two details are different sentences, not one sentence with a different prefix.
    expect(withNull.detail).not.toBe(withoutData.detail);
  });
});

// ---------------------------------------------------------------------------
// 2 · The level that disagreed says which one it was
// ---------------------------------------------------------------------------
describe('W48-2 · `biz_data` there but `chat_sessions` not an array', () => {
  it('names `data.biz_data` and what is in it instead, and still halts without archiving', async () => {
    const store = memoryStore();
    const renamed = JSON.stringify({
      code: 0,
      msg: 'ok',
      data: { biz_code: 0, biz_msg: 'ok', biz_data: { sessions: [session(1)], has_more: false } },
    });
    const be = backend([renamed]);

    const report = await run(store, be, SCOPES.notArray);

    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('no `data.biz_data.chat_sessions` array');
    expect(report.halted?.detail).toContain('[saw data.biz_data: {has_more:boolean, sessions:array(1)}]');
    expect(report.newDebts).toBe(0);
    expect(report.state.enumCursor.complete).toBe(false);
  });

  it('a `chat_sessions` that is present but the wrong type reports that type, not "absent"', () => {
    const asNumber = parseDeepSeekListPage(
      JSON.stringify({ code: 0, data: { biz_data: { chat_sessions: 0, has_more: false } } }),
    );
    expect(asNumber.ok).toBe(false);
    if (asNumber.ok) throw new Error('a non-array chat_sessions must not parse');
    expect(asNumber.detail).toContain('[saw data.biz_data: {chat_sessions:number, has_more:boolean}]');

    const asObject = parseDeepSeekListPage(
      JSON.stringify({ code: 0, data: { biz_data: { chat_sessions: { page: 1, rows: [] } } } }),
    );
    expect(asObject.ok).toBe(false);
    if (asObject.ok) throw new Error('an object chat_sessions must not parse');
    expect(asObject.detail).toContain('[saw data.biz_data: {chat_sessions:{page:number, rows:array(0)}}]');
  });
});

// ---------------------------------------------------------------------------
// 3 · The body parsers, same vocabulary
// ---------------------------------------------------------------------------
describe('W48-3 · the DeepSeek detail parsers name what they saw too', () => {
  it('a renamed `chat_messages` reports the level and the sibling that replaced it', () => {
    const renamed = JSON.stringify({
      code: 0,
      msg: 'ok',
      data: { biz_code: 0, biz_msg: 'ok', biz_data: { chat_session: { id: 'ds-0001-aaaaaaaa' }, messages: [] } },
    });
    const page = parseDeepSeekDetailPage(renamed);
    expect(page.ok).toBe(false);
    if (page.ok) throw new Error('a renamed chat_messages must not parse');
    expect(page.detail).toContain('no `data.biz_data.chat_messages` array');
    expect(page.detail).toContain('[saw data.biz_data: {chat_session:{id:string}, messages:array(0)}]');

    // The tree walk refuses the same body, in its own words, with the same evidence.
    const walked = parseDeepSeekDetailTree(renamed);
    expect(walked).toEqual({
      ok: false,
      kind: 'unreadable',
      detail:
        'the detail response has no `data.biz_data.chat_messages` array'
        + ' [saw data.biz_data: {chat_session:{id:string}, messages:array(0)}]',
    });
  });

  it('a `chat_session` with no readable leaf shows the keys it does have', () => {
    const drifted = JSON.stringify({
      data: { biz_data: { chat_messages: [msg(1, null)], chat_session: { session_id: 7, title: 'synthetic-fixture' } } },
    });
    const walked = parseDeepSeekDetailTree(drifted);
    expect(walked.ok).toBe(false);
    if (walked.ok) throw new Error('a session without a leaf must not walk');
    expect(walked.kind).toBe('unreadable');
    // 🔴 The key `title` is schema and is echoed; the title itself is a value and is not.
    expect(walked.detail).toContain('[saw chat_session: {session_id:number, title:string}]');
  });

  it('a message that is not an object reports the type that arrived', () => {
    const drifted = JSON.stringify({
      data: { biz_data: { chat_messages: [msg(1, null), 7], chat_session: { current_message_id: 1 } } },
    });
    const walked = parseDeepSeekDetailTree(drifted);
    expect(walked.ok).toBe(false);
    if (walked.ok) throw new Error('a non-object message must not walk');
    expect(walked.detail).toContain('[saw message: number]');
  });

  it('a body that is not an object at all reports the top level', () => {
    const page = parseDeepSeekDetailPage('[]');
    expect(page.ok).toBe(false);
    if (page.ok) throw new Error('a top-level array is not a body');
    expect(page.detail).toContain('[saw body: array(0)]');
  });
});

// ---------------------------------------------------------------------------
// 4 · The control: more said, nothing accepted
// ---------------------------------------------------------------------------
describe('W48-4 · a well-formed response is untouched', () => {
  it('the same page and body that parsed before still parse, and the leg still archives', async () => {
    const wellFormed = JSON.stringify({
      code: 0,
      msg: 'ok',
      data: { biz_code: 0, biz_msg: 'ok', biz_data: { chat_sessions: [session(1)], has_more: false } },
    });

    const parsed = parseDeepSeekListPage(wellFormed);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) throw new Error('the well-formed page must parse');
    expect(parsed.page.ids).toEqual(['ds-0001-aaaaaaaa']);
    expect(parsed.page.nextCursor).toBe(999);
    expect(parsed.page.hasMore).toBe(false);
    // 🔴 Nothing was appended to a successful parse: the marker is only on a refusal.
    expect(parsed).not.toHaveProperty('detail');

    expect(parseDeepSeekDetailPage(wholeBody())).toEqual({ ok: true, outcome: 'non-empty' });

    // End to end: the leg lists, fetches the body, checks the tree and archives it.
    const store = memoryStore();
    const be = backend([wellFormed], wholeBody());
    const report = await run(store, be, SCOPES.control, { maxDetails: 1 });

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.archivedThisRun).toEqual(['ds-0001-aaaaaaaa']);
    expect(report.state.archived).toEqual(['ds-0001-aaaaaaaa']);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 5 · 🔴 The privacy line: the structure travels, the content does not
// ---------------------------------------------------------------------------
describe('W48-5 · no value out of the response ever reaches the detail', () => {
  it('an id, a title and a message body in the response appear nowhere in the refusal', () => {
    const page = JSON.stringify({
      code: 0,
      msg: 'ok',
      data: {
        biz_code: 0,
        biz_msg: 'ok',
        biz_data: {
          chat_sessions: [
            // A plausible row that drifted in the one field the parser reads a type from.
            { id: PRIVATE_ID, seq_id: 993, updated_at: PRIVATE_TITLE, title: PRIVATE_TITLE, content: PRIVATE_TEXT },
          ],
          has_more: false,
        },
      },
    });

    const parsed = parseDeepSeekListPage(page);
    expect(parsed.ok).toBe(false);
    if (parsed.ok) throw new Error('a non-numeric updated_at must not parse');

    // What the diagnosis needs is here...
    expect(parsed.detail).toContain('non-numeric `updated_at`');
    expect(parsed.detail).toContain('[saw item: {content:string, id:string, seq_id:number, title:string, updated_at:string}]');
    // ...and not one value out of the response is.
    expect(parsed.detail).not.toContain(PRIVATE_ID);
    expect(parsed.detail).not.toContain(PRIVATE_TITLE);
    expect(parsed.detail).not.toContain(PRIVATE_TEXT);
    // Not even a fragment of a value, which a prefix or a length would be.
    expect(parsed.detail).not.toContain('Kyoto');
    expect(parsed.detail).not.toContain(String(993));
    expect(parsed.detail).not.toContain('1755');
  });

  it('a key that is itself content is withheld, and the structure around it still shows', () => {
    // A response keyed by what it holds — a title, a hyphenated id, an opaque token — is a
    // shape that exists in the wild, so "keys are schema" cannot be assumed.
    const page = JSON.stringify({
      code: 0,
      msg: 'ok',
      data: {
        biz_code: 0,
        biz_msg: 'ok',
        biz_data: {
          [PRIVATE_TITLE]: [{ id: PRIVATE_ID }],
          [PRIVATE_ID]: [{ id: PRIVATE_ID }],
          [PRIVATE_TOKEN]: [{}],
          has_more: false,
        },
      },
    });

    const parsed = parseDeepSeekListPage(page);
    expect(parsed.ok).toBe(false);
    if (parsed.ok) throw new Error('no chat_sessions array must not parse');

    // 🔴 The exact bracket is pinned: three keys withheld and visibly so, the one schema
    //    key beside them named, and the lengths kept — the count is a measurement of the
    //    envelope, which is the whole point of being allowed to say it.
    expect(parsed.detail).toContain(
      `[saw data.biz_data: {${WITHHELD}:array(1), ${WITHHELD}:array(1), ${WITHHELD}:array(1), has_more:boolean}]`,
    );
    expect(parsed.detail).not.toContain(PRIVATE_TITLE);
    expect(parsed.detail).not.toContain(PRIVATE_ID);
    expect(parsed.detail).not.toContain(PRIVATE_TOKEN);
    expect(parsed.detail).not.toContain('Kyoto');
  });
});
