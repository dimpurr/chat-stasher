/**
 * W42 · **Does a long DeepSeek conversation come back whole?**
 *
 * The question is not answered by any source (see the W42 report §2 — no reviewed
 * implementation sends a paging parameter on the body request, and the complete
 * body envelope carries no total, no `has_more` and no page token). What W42
 * settles instead is the thing that was actually dangerous: before it,
 * `DEEPSEEK_PLAN` declared **no** `parseDetailPage`, so a body went from the shape
 * gate straight into the sink through the same path as a Gemini bundle the paging
 * loop had walked to the end and proved whole. The archive could not tell them
 * apart, so a truncated conversation would have been stored and its debt settled.
 *
 * `parseDeepSeekDetailPage` closes that with the instrument the measured envelope
 * does supply: a tree. `chat_session.current_message_id` names the newest message
 * of the branch the user was looking at, every message names its `parent_id`, and
 * the visible branch is the chain between them. A body is archived only when that
 * chain closes at a root. This file pins the three answers a body can get, and —
 * the part that is easiest to get wrong — the difference between **a conversation
 * that was checked and found short** and **a body whose pointers were not there to
 * check with**. The first is a per-conversation failure; the second is a halt,
 * because a wrong field name must never become a leg's worth of "this
 * conversation is incomplete" verdicts about conversations nobody walked.
 *
 * 🔴 Every body below is **synthetic**, written by hand from the measured field
 *    names (~2026-09-13 logged-in session; the same names this repository records
 *    at tests/unsupported-transport.test.ts:141). No request goes to
 *    deepseek.com, there is no login, and no real conversation appears. The http
 *    port is always injected.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { t } from '../lib/i18n';
import { describeFailureReason } from '../lib/backfill/failures';
import {
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_LIST_PATH,
  parseDeepSeekDetailPage,
  parseDeepSeekDetailTree,
} from '../lib/backfill/enumerate';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chat.deepseek.com';
const ID = 'ds-0001-aaaaaaaa';
const ID2 = 'ds-0002-aaaaaaaa';
const SCOPES = { whole: 'acct-w42-whole', mixed: 'acct-w42-mixed', unknown: 'acct-w42-unknown' } as const;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock {
  let t = Date.parse('2026-09-19T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

/** One synthetic message in the measured shape: a numeric `message_id` and a `parent_id` that is null at a root. */
function msg(messageId: number, parentId: number | null): Record<string, unknown> {
  return {
    message_id: messageId,
    parent_id: parentId,
    role: messageId % 2 === 1 ? 'USER' : 'ASSISTANT',
    status: 'FINISHED',
    fragments: [{ id: messageId, type: messageId % 2 === 1 ? 'REQUEST' : 'RESPONSE', content: `synthetic-${messageId}` }],
  };
}

/**
 * A synthetic body: the measured envelope, with the session's current leaf and the
 * messages handed in. `sessionExtra` is how a body whose tree pointers are *not
 * readable* is built — the case that must halt rather than accuse.
 */
function body(
  messages: Record<string, unknown>[],
  opts: { current?: unknown; omitCurrent?: boolean; session?: Record<string, unknown> } = {},
): string {
  const session: Record<string, unknown> = { id: ID, title: 'synthetic-fixture', ...opts.session };
  if (!opts.omitCurrent) session.current_message_id = opts.current === undefined ? null : opts.current;
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: { biz_code: 0, biz_msg: 'ok', biz_data: { chat_session: session, chat_messages: messages } },
  });
}

/** A whole conversation: root-first, every link resolvable, the leaf at the end. */
function wholeBody(leaf = 4): string {
  const messages = [msg(1, null), msg(2, 1), msg(3, 2), msg(4, 3)];
  return body(messages, { current: leaf });
}

// ---------------------------------------------------------------------------
// 1 · The walk itself: what proves a body whole, and what refuses it
// ---------------------------------------------------------------------------
describe('W42-1 · the completeness walk reads the response, and refuses three different things', () => {
  it('a chain that closes at a root is the whole visible branch', () => {
    const verdict = parseDeepSeekDetailTree(wholeBody());
    expect(verdict).toEqual({ ok: true });
    expect(parseDeepSeekDetailPage(wholeBody())).toEqual({ ok: true, outcome: 'non-empty' });
  });

  it('truncated to the NEWEST messages ⇒ incomplete: the chain upward leaves the response', () => {
    // What a "history_messages" endpoint serving the most recent context would produce: the newest
    // turns are all there, and the messages they descend from are not.
    const bytes = body([msg(3, 2), msg(4, 3)], { current: 4 });
    expect(parseDeepSeekDetailTree(bytes)).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'the parent chain leaves the messages this response carries',
    });
    // 🔴 The one outcome that must never appear: archived as a complete conversation.
    expect(parseDeepSeekDetailPage(bytes)).toEqual({ ok: true, outcome: 'detail-tree-incomplete' });
  });

  it('truncated to the OLDEST messages ⇒ incomplete: the current leaf is not in the response at all', () => {
    // The failure mode the plan names in so many words ("may come back as only its first part").
    const bytes = body([msg(1, null), msg(2, 1)], { current: 4 });
    expect(parseDeepSeekDetailTree(bytes)).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'the current message is not among the chat messages this response carries',
    });
    expect(parseDeepSeekDetailPage(bytes)).toEqual({ ok: true, outcome: 'detail-tree-incomplete' });
  });

  it('a chain that never reaches a root is not whole, however much content it carries', () => {
    // Every message present, every link resolvable, and a cycle instead of a root: a shape this code
    // cannot read as a branch. It is reported as incomplete, never as a root.
    const bytes = body([msg(1, 3), msg(2, 1), msg(3, 2)], { current: 3 });
    expect(parseDeepSeekDetailTree(bytes)).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'the parent chain revisits a message',
    });
    // And the neighboring checks, each of which must also refuse rather than pass:
    // two messages claiming one identity would make the walk resolve a link to the wrong node.
    expect(parseDeepSeekDetailTree(body([msg(1, null), msg(1, null)], { current: 1 }))).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'two chat messages carry the same `message_id`',
    });
    // A parent link that is neither null (root) nor a number (link) may NOT be read as a root: that
    // would let a response this code cannot read through as a whole conversation.
    const unreadableLink = body(
      [{ message_id: 1, parent_id: '', role: 'USER' }, msg(2, 1)],
      { current: 2 },
    );
    expect(parseDeepSeekDetailTree(unreadableLink)).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'a chat message carries a `parent_id` this code cannot read',
    });
  });
});

// ---------------------------------------------------------------------------
// 2 · 🔴 The split that matters: "checked and short" is not "could not check"
// ---------------------------------------------------------------------------
describe('W42-2 · a body we could not check is never reported as a conversation that came back short', () => {
  it('no readable current_message_id ⇒ {ok:false} ⇒ halt, not a per-conversation verdict', () => {
    // The field is gone entirely.
    expect(parseDeepSeekDetailTree(body([msg(1, null), msg(2, 1)], { omitCurrent: true }))).toEqual({
      ok: false,
      kind: 'unreadable',
      detail: 'the detail response names no numeric `chat_session.current_message_id`',
    });
    // 🔴 Present but of the wrong type is the same answer, deliberately: the same rule
    //    parseDeepSeekListPage applies to a non-numeric `updated_at`. A type change is a wire change.
    expect(parseDeepSeekDetailTree(body([msg(1, null), msg(2, 1)], { current: '2' })))
      .toEqual({ ok: false, kind: 'unreadable', detail: 'the detail response names no numeric `chat_session.current_message_id`' });
    // And at the parser level it is `{ok:false}`, which the engine turns into halt('shape-changed') —
    // never into 'detail-tree-incomplete' about a conversation whose branch was never walked.
    const parsed = parseDeepSeekDetailPage(body([msg(1, null), msg(2, 1)], { omitCurrent: true }));
    expect(parsed.ok).toBe(false);
    if (parsed.ok) return;
    expect(parsed.detail).toContain('current_message_id');
  });

  it('the envelope itself unreadable ⇒ {ok:false}, never a verdict', () => {
    const cases: { bytes: string; contains: string }[] = [
      { bytes: 'not json', contains: 'is not JSON' },
      { bytes: '[]', contains: 'is not a JSON object' },
      { bytes: JSON.stringify({ data: {} }), contains: 'data.biz_data' },
      // `chat_messages` present and non-empty, and no `chat_session` to read the leaf from.
      {
        bytes: JSON.stringify({ data: { biz_data: { chat_messages: [msg(1, null)] } } }),
        contains: 'chat_session',
      },
      // 🔴 An empty array is NOT in this list: it is a readable shape with a real answer of its own,
      //    asserted in section 3.
      { bytes: body([msg(1, null)], { omitCurrent: true }), contains: 'current_message_id' },
    ];
    for (const { bytes, contains } of cases) {
      const parsed = parseDeepSeekDetailPage(bytes);
      expect(parsed.ok, bytes).toBe(false);
      if (parsed.ok) continue;
      expect(parsed.detail, bytes).toContain(contains);
    }
    // Control: the same builder with the pointers present parses, so the cases above fail on the
    // missing pointer and not because the fixture is malformed in some other way.
    expect(parseDeepSeekDetailPage(wholeBody())).toEqual({ ok: true, outcome: 'non-empty' });
  });
});

// ---------------------------------------------------------------------------
// 3 · 🔴 The ambiguous body takes the unknown path, never the empty one
// ---------------------------------------------------------------------------
describe('W42-3 · an empty body is read as "we cannot tell", never as "there was nothing"', () => {
  it('an empty chat_messages array ⇒ detail-empty-unverified (a receipt with complete:false)', () => {
    const bytes = body([], { omitCurrent: true });
    // The walk is never even reached: with no messages there is nothing to walk, and the question the
    // response answers is not "is this branch closed" but "was this conversation ever non-empty".
    expect(parseDeepSeekDetailPage(bytes)).toEqual({ ok: true, outcome: 'detail-empty-unverified' });
    // 🔴 And specifically NOT 'non-empty': "this conversation has no messages" and "this response is a
    //    window with nothing in it" are indistinguishable from this payload alone.
    expect(parseDeepSeekDetailPage(bytes)).not.toEqual({ ok: true, outcome: 'non-empty' });
  });
});

// ---------------------------------------------------------------------------
// 4 · End to end: what the leg does with each of the three answers
// ---------------------------------------------------------------------------
function listPage(ids: string[]): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_data: {
        chat_sessions: ids.map((id, i) => ({ id, seq_id: 1000 - i, updated_at: 1_755_000_000 + i })),
        has_more: false,
      },
    },
  });
}

function backend(byId: Record<string, string>): { http: (url: string, init?: unknown) => Promise<HttpResponse>; calls: string[] } {
  const calls: string[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === DEEPSEEK_LIST_PATH) return { status: 200, text: listPage(Object.keys(byId)) };
    if (u.pathname === DEEPSEEK_DETAIL_PATH) {
      const id = u.searchParams.get('chat_session_id') ?? '';
      const bytes = byId[id];
      if (bytes === undefined) throw new Error(`unexpected conversation ${id}`);
      return { status: 200, text: bytes };
    }
    throw new Error(`unexpected path ${u.pathname}`);
  };
  return { calls, http };
}

function run(store: ReturnType<typeof memoryStore>, be: { http: (url: string, init?: unknown) => Promise<HttpResponse> }, scope: string) {
  return runBackfill({
    platform: 'deepseek',
    origin: ORIGIN,
    scope,
    store,
    http: be.http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured) => ({ saved: true, sessionId: captured.sessionId }),
  });
}

describe('W42-4 · the leg archives a whole body and refuses a short one, in the same run', () => {
  it('a body whose branch closes is archived and its debt settled', async () => {
    const store = memoryStore();
    const be = backend({ [ID]: wholeBody() });
    const report = await run(store, be, SCOPES.whole);

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.archivedThisRun).toEqual([ID]);
    expect(report.state.archived).toEqual([ID]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
  });

  it('a truncated body is refused by name while the whole conversation beside it is still archived', async () => {
    const store = memoryStore();
    // Two conversations, same run: one whose current leaf is outside the response (truncated to the
    // oldest turns), one whole. The point is the *combination* — the leg must carry on, not halt.
    const be = backend({
      [ID]: body([msg(1, null), msg(2, 1)], { current: 4 }),
      [ID2]: wholeBody(),
    });
    const report = await run(store, be, SCOPES.mixed);

    // 🔴 The truncated conversation is NOT in the archive and its debt is not settled: storing it would
    //    put a partial answer in the archive with nothing marking it partial.
    expect(report.archivedThisRun).toEqual([ID2]);
    expect(report.state.archived).toEqual([ID2]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-tree-incomplete']);
    expect(report.state.failures?.map((f) => f.reason)).toEqual(['detail-tree-incomplete']);
    // The receipt locates the conversation without carrying it: the first 8 characters, platform, when.
    expect(report.state.failures?.[0]!.shortId).toBe(ID.slice(0, 8));
    expect(report.state.failures?.[0]!.platform).toBe('deepseek');
    // The request really went out, so it counts against the day's quota like any other body.
    expect(report.state.detailToday.count).toBe(2);
    // 🔴 Not a halt: a long conversation is long, and the short ones beside it are complete.
    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    // And the reason reads as an observed fact in the user's language, not as a reason code.
    expect(describeFailureReason('detail-tree-incomplete')).toBe(t('failure.detailTreeIncomplete'));
    expect(describeFailureReason('detail-tree-incomplete')).not.toContain('detail-tree-incomplete');
  });

  it('a body whose tree pointers are gone halts the leg and leaves every debt in place', async () => {
    const store = memoryStore();
    const be = backend({
      [ID]: body([msg(1, null), msg(2, 1)], { omitCurrent: true }),
      [ID2]: wholeBody(),
    });
    const report = await run(store, be, SCOPES.unknown);

    // 🔴 A halt, not two per-conversation failures: the pointers the check reads are not in the
    //    response, so nothing was checked, and "this conversation is incomplete" is not a statement
    //    this code may make about a conversation it never walked.
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('current_message_id');
    expect(report.archivedThisRun).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
    // No debt was written off, so a corrected parser resumes exactly this work instead of the
    // conversations being counted as handled.
    expect(report.state.pending).toEqual([ID, ID2]);
    // The leg stopped at the first body, and it stopped *before* the conversation that would have
    // been fine: a halt is about the wire, not about which conversation happened to come first.
    expect(be.calls.length).toBe(2);
  });
});
