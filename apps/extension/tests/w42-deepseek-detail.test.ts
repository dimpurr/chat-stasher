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
const SCOPES = {
  whole: 'acct-w42-whole',
  mixed: 'acct-w42-mixed',
  unknown: 'acct-w42-unknown',
  siblings: 'acct-w42-siblings',
  stringIds: 'acct-w42-stringids',
} as const;

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
  messages: unknown[],
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

/**
 * 🔴 W42b · The shape the endpoint **actually** returns: a whole conversation that
 * also carries the branches the user did not take. The walk follows one branch —
 * the chain from `current_message_id` back to a root — and everything off that
 * chain is carried along in the archived bytes but never visited. 9→2 and 10→9 are
 * a discarded sibling branch that resolves; 11→77 is a discarded sibling whose own
 * parent is not in the response at all, which is exactly what a caller must not
 * mistake for a broken chain.
 */
function branchingBody(leaf = 4): string {
  return body(
    [msg(1, null), msg(2, 1), msg(3, 2), msg(4, 3), msg(9, 2), msg(10, 9), msg(11, 77)],
    { current: leaf },
  );
}

/** The measured shape with one upstream type change: every `message_id` arrives as a string. */
function stringIdBody(): string {
  return body(
    [
      { message_id: '1', parent_id: null, role: 'USER' },
      { message_id: '2', parent_id: '1', role: 'ASSISTANT' },
    ],
    { current: 2 },
  );
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

  it('a body carrying discarded sibling branches is whole: the walk follows one branch, not the tree', () => {
    // 🔴 W42b · The endpoint returns a tree, not a chain — the visible branch plus the branches the
    //    user did not take (blueprint.md:235). `branchingBody()` carries both a sibling that resolves
    //    (9→2, 10→9) and one whose own parent is absent from the response (11→77). Both are off the
    //    chain the walk follows, so neither may be visited, and neither may turn a whole conversation
    //    into a refusal. A walk that validated every message's link, or that read a stranded sibling
    //    as "the chain leaves the response", would fail this and nothing else in the file.
    expect(parseDeepSeekDetailTree(branchingBody())).toEqual({ ok: true });
    expect(parseDeepSeekDetailPage(branchingBody())).toEqual({ ok: true, outcome: 'non-empty' });
    // The chain itself is what is walked, so a gap in it still refuses the body with the siblings
    // present — the fixture is the measured shape, not a weaker one.
    const withGapInTheChain = body(
      [msg(1, null), msg(2, 1), msg(4, 3), msg(9, 2), msg(10, 9), msg(11, 77)],
      { current: 4 },
    );
    expect(parseDeepSeekDetailTree(withGapInTheChain)).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'the parent chain leaves the messages this response carries',
    });
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

  it('a `message_id` of the wrong type ⇒ {ok:false} ⇒ halt, not a debt written off', () => {
    // 🔴 W42b · One upstream type change (ids arriving as strings) must not become a
    //    per-conversation "this conversation is incomplete" for **every** conversation. That verdict
    //    is not a halt: the engine writes the debt off (`engine.ts`, 'detail-tree-incomplete'), so a
    //    changed type would silently count every conversation as handled, with no retry and nothing
    //    archived. Nothing was walked here, so it is the wire that changed and the leg that stops.
    expect(parseDeepSeekDetailTree(stringIdBody())).toEqual({
      ok: false,
      kind: 'unreadable',
      detail: 'a chat message carries no numeric `message_id`',
    });
    const parsed = parseDeepSeekDetailPage(stringIdBody());
    expect(parsed.ok).toBe(false);
    if (parsed.ok) return;
    expect(parsed.detail).toContain('message_id');
    expect(parseDeepSeekDetailPage(stringIdBody()))
      .not.toEqual({ ok: true, outcome: 'detail-tree-incomplete' });

    // A chat message that is not an object at all is the same answer, for the same reason.
    const notAnObject = body([null, msg(2, 1)], { current: 2 });
    expect(parseDeepSeekDetailTree(notAnObject)).toEqual({
      ok: false,
      kind: 'unreadable',
      detail: 'a chat message is not an object',
    });
    expect(parseDeepSeekDetailPage(notAnObject).ok).toBe(false);
  });

  it('a genuinely broken chain is still refused per conversation: readable ids, unclosed walk', () => {
    // The line the fix draws, asserted from the other side. Every id here is readable and every
    // message is an object, so the walk really ran — and it is the walk, not a type change, that did
    // not close. These stay 'detail-tree-incomplete' (nothing archived, a named failure, the leg
    // carries on); halting on them instead would stop a run for a long conversation.
    const brokenChains: { bytes: string; detail: string }[] = [
      { bytes: body([msg(3, 2), msg(4, 3)], { current: 4 }), detail: 'the parent chain leaves the messages this response carries' },
      { bytes: body([msg(1, null), msg(2, 1)], { current: 9 }), detail: 'the current message is not among the chat messages this response carries' },
      { bytes: body([msg(1, 3), msg(2, 1), msg(3, 2)], { current: 3 }), detail: 'the parent chain revisits a message' },
    ];
    for (const { bytes, detail } of brokenChains) {
      expect(parseDeepSeekDetailTree(bytes)).toEqual({ ok: false, kind: 'incomplete', detail });
      expect(parseDeepSeekDetailPage(bytes)).toEqual({ ok: true, outcome: 'detail-tree-incomplete' });
    }
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

  it('a body that also carries discarded sibling branches is archived, siblings and all', async () => {
    // 🔴 W42b · The measured shape, end to end: the archived bytes are the response byte for byte, so
    //    the discarded branches travel with it — and the walk that decides "whole" follows the one
    //    chain and never visits them.
    const store = memoryStore();
    const be = backend({ [ID]: branchingBody() });
    const report = await run(store, be, SCOPES.siblings);

    expect(report.halted).toBeNull();
    expect(report.stopped).toBe('queue-empty');
    expect(report.archivedThisRun).toEqual([ID]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
  });

  it('a body whose message ids arrive as strings halts, so no debt is written off', async () => {
    const store = memoryStore();
    const be = backend({ [ID2]: stringIdBody(), [ID]: stringIdBody() });
    const report = await run(store, be, SCOPES.stringIds);

    // 🔴 W42b · Before this, a string id was 'detail-tree-incomplete': every conversation in the run
    //    took that outcome, the debt was dropped, and a later parser fix had nothing left to resume.
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('message_id');
    expect(report.archivedThisRun).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
    expect(report.state.failures ?? []).toEqual([]);
    expect(report.state.pending).toEqual([ID2, ID]);
    // The leg stopped at the first body, before the conversation behind it: a halt is about the wire.
    expect(be.calls.length).toBe(2);
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
