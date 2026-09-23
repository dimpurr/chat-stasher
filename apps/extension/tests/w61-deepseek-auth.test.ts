/**
 * W61 · DeepSeek (chat.deepseek.com) refuses **in-band**, and the leg asked for no
 * token — so a login refusal was recorded as a wire-shape change.
 *
 * ## The measured defect
 * A cookie-only `GET /api/v0/chat_session/fetch_page?count=N` answers **HTTP 200**
 * with `{code: 40002, data: null, msg: "Missing Token"}`; a cookie-only
 * `GET /api/v0/chat/history_messages?chat_session_id=<id>` answers **HTTP 200**
 * with `{code: 40003, data: null, msg: "INVALID_TOKEN"}`. The same two requests
 * with `authorization: Bearer <userToken.value>` answer `code: 0`,
 * `data.biz_code: 0` and the data the parsers expect. Measured from the page's own
 * context in a logged-in Chrome, 2026-09-23 — the probe table is in the W61 report.
 *
 * Our parsers read only `data`, saw `null`, and the leg halted **`shape-changed`**
 * with "deepseek list response has no `data` object (envelope changed?)". The user
 * was told the API had changed.
 *
 * ## What this file exists to stop changing back
 *  1. **Both segments halt `auth-refused`, never `shape-changed`**, when the
 *     platform answers with a refusal envelope — including the body segment, where
 *     `matchesResponseShape` runs *before* the plan's parser and would otherwise
 *     fire first. That ordering is the whole reason the refusal is declared on the
 *     plan (`BackfillEnumPlan.refusalOf`) rather than checked inside the list
 *     parser.
 *  2. **A refusal is never "you have no conversations".** Zero debts, nothing
 *     archived, and `enumTruncated` saying the list was never read rather than
 *     saying it was read and empty.
 *  3. **The bearer goes to two paths and nowhere else.** Read from the page's own
 *     storage at request time, kept in no variable, attached only to DeepSeek's
 *     list and body paths on a DeepSeek origin, re-read once after a 401/403, and
 *     omitted entirely when there is no token — so the platform's own refusal is
 *     what the leg reads instead of an invented one.
 *  4. **`userToken` is a JSON object and the token is its `.value`.** The stored
 *     string is never itself sent as a header.
 *  5. **Nothing else got less strict.** A non-zero code is refused *before* any
 *     shape judgement and never instead of one; a `code` that is not a number is
 *     left to the shape checks; a well-formed envelope goes through byte for byte
 *     as before.
 *
 * 🔴 All fixtures are **synthetic**, written from the key names the probe measured.
 *    No request goes to chat.deepseek.com, there is no logged-in state, and no real
 *    conversation, account, token or id appears anywhere below. The http port is
 *    always injected and always throws on a path it was not given, so "no request
 *    was sent" is proven by the run rather than asserted about it.
 */

import { describe, expect, it, vi } from 'vitest';
import { matchesResponseShape, PLATFORMS } from '../lib/contract';
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  CHATGPT_PLAN,
  CLAUDE_PLAN,
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_LIST_PATH,
  DEEPSEEK_PLAN,
  GEMINI_PLAN,
  GROK_PLAN,
  KIMI_PLAN,
  PERPLEXITY_PLAN,
  deepSeekEnvelopeRefusal,
  parseDeepSeekListPage,
} from '../lib/backfill/enumerate';
import {
  createDeepSeekAuthorizedFetch,
  DEEPSEEK_USER_TOKEN_STORAGE_KEY,
  needsDeepSeekBearer,
  readDeepSeekUserToken,
  type MinimalResponse,
} from '../lib/platform-auth';
import { t } from '../lib/i18n';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chat.deepseek.com';
const LIST_URL = `${ORIGIN}${DEEPSEEK_LIST_PATH}?count=100`;
const ID = 'b7f1c2d3-e4a5-4b6c-8d7e-9f0a1b2c3d4e';
const DETAIL_URL = `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?chat_session_id=${encodeURIComponent(ID)}`;
const PAGE_URL = `${ORIGIN}/a/chat/s/${ID}`;
/** A synthetic token. Never the real thing: the probe's was 92 characters of JSON around a JWT. */
const TOKEN = 'synthetic.deepseek.token';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fake clock that advances on sleep, so a run's timing is assertable rather than sampled. */
function fakeClock(): Clock & { sleeps: number[] } {
  const sleeps: number[] = [];
  let time = Date.parse('2026-09-23T00:00:00.000Z');
  return {
    sleeps,
    now: () => time,
    async sleep(ms: number) {
      sleeps.push(ms);
      time += ms;
    },
  };
}

// ---------------------------------------------------------------------------
// Synthetic fixtures — key names from the probe, values invented here
// ---------------------------------------------------------------------------

/** One list row in the measured key set. */
function session(id: string, seqId: number): Record<string, unknown> {
  return {
    id,
    seq_id: seqId,
    updated_at: 1758600000000,
    title: 'synthetic title',
    title_type: 'chat',
    model_type: 'default',
    pinned: false,
    inserted_at: 1758500000000,
    version: 1,
    agent: 'chat',
    current_message_id: 2,
  };
}

/** A **good** list envelope: `code: 0`, `data.biz_code: 0`, one session, no next page. */
function goodListPage(ids: string[]): string {
  return JSON.stringify({
    code: 0,
    msg: '',
    data: {
      biz_code: 0,
      biz_msg: '',
      biz_data: {
        chat_sessions: ids.map((id, i) => session(id, ids.length - i)),
        has_more: false,
      },
    },
  });
}

/**
 * A **good** body envelope: two messages whose parent chain closes at a root, with
 * `chat_session.current_message_id` naming the leaf. Every pointer is a JSON
 * number, which is what W42's walk requires.
 */
function goodDetailPage(): string {
  return JSON.stringify({
    code: 0,
    msg: '',
    data: {
      biz_code: 0,
      biz_msg: '',
      biz_data: {
        cache_control: 'synthetic',
        cache_reset_at: 1758600000000,
        chat_messages: [
          { message_id: 1, parent_id: null, role: 'USER', status: 'FINISHED', inserted_at: 1758500000000 },
          { message_id: 2, parent_id: 1, role: 'ASSISTANT', status: 'FINISHED', inserted_at: 1758500001000 },
        ],
        chat_session: { id: ID, current_message_id: 2 },
      },
    },
  });
}

/** The measured **list** refusal: HTTP 200, `data: null`, the code in the envelope. */
const LIST_REFUSAL = JSON.stringify({ code: 40002, data: null, msg: 'Missing Token' });
/** The measured **body** refusal: the same shape, a different code. */
const DETAIL_REFUSAL = JSON.stringify({ code: 40003, data: null, msg: 'INVALID_TOKEN' });

interface Recorder {
  calls: { url: string; init: unknown; at: number }[];
}

/**
 * A synthetic backend. Any path it was not given **throws**, so "this leg sent no
 * further request" is proven by the run rather than claimed.
 */
function backend(
  clock: Clock,
  routes: Partial<Record<string, string | ((u: URL) => string)>>,
): Recorder & { http: (url: string, init?: unknown) => Promise<HttpResponse> } {
  const calls: { url: string; init: unknown; at: number }[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push({ url, init, at: clock.now() });
    const u = new URL(url);
    const route = routes[u.pathname];
    if (route === undefined) throw new Error(`unexpected path ${u.pathname}`);
    return { status: 200, text: typeof route === 'string' ? route : route(u) };
  };
  return { calls, http };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'deepseek',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
    ...extra,
  });
}

// ---------------------------------------------------------------------------
// 1 · The defect itself: a refusal envelope is not a shape change
// ---------------------------------------------------------------------------

describe('W61-1 · a DeepSeek refusal halts auth-refused, on both segments', () => {
  it('the LIST segment: HTTP 200 + code 40002 is a refusal, not a wire change', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const be = backend(clock, { [DEEPSEEK_LIST_PATH]: LIST_REFUSAL });

    const report = await run(store, be.http, 'default');

    // 🔴 The assertion this file exists for. Before W61 this was 'shape-changed'
    //    with "no `data` object (envelope changed?)" — an accurate-sounding lie
    //    about a platform that had said "Missing Token".
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.detail).toContain('code 40002');
    expect(report.halted?.detail).toContain('Missing Token');
    // The old sentence is gone, and specifically not merely reformatted.
    expect(report.halted?.detail).not.toContain('envelope changed');
    // Exactly one request: the leg stopped at the list and never built a body URL.
    expect(be.calls).toHaveLength(1);
    expect(be.calls[0]!.url).toBe(LIST_URL);
  });

  it('the BODY segment: the refusal is read before the contract shape gate can preempt it', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const be = backend(clock, {
      [DEEPSEEK_LIST_PATH]: goodListPage([ID]),
      [DEEPSEEK_DETAIL_PATH]: DETAIL_REFUSAL,
    });

    const report = await run(store, be.http, 'default');

    // 🔴 This is the case `BackfillEnumPlan.refusalOf` exists for. A refusal has no
    //    `data`, so it fails the contract row's requiredAnyPaths; the engine runs
    //    `matchesResponseShape` BEFORE the plan's parser, so without the plan-level
    //    hook the body leg would report 'shape-changed' with "does not match the
    //    deepseek response shape" — while the platform had said "INVALID_TOKEN".
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('auth-refused');
    expect(report.halted?.detail).toContain('detail body:');
    expect(report.halted?.detail).toContain('code 40003');
    expect(report.halted?.detail).toContain('INVALID_TOKEN');
    expect(report.halted?.detail).not.toContain('does not match');
    expect(be.calls.map((c) => new URL(c.url).pathname)).toEqual([
      DEEPSEEK_LIST_PATH,
      DEEPSEEK_DETAIL_PATH,
    ]);
  });

  it('a refusal is never "you have no conversations": nothing listed, nothing archived', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const be = backend(clock, { [DEEPSEEK_LIST_PATH]: LIST_REFUSAL });

    const report = await run(store, be.http, 'default');

    // 🔴 CLAUDE.md invariant 1. Zero is a measurement; here there was no
    //    measurement at all, so the run may not present one.
    expect(report.newDebts).toBe(0);
    expect(report.archivedThisRun).toEqual([]);
    expect(report.enumeratedPages).toBe(0);
    // The named record of "the list was never read" — not a completed cursor.
    expect(report.enumTruncated).toBeNull();
    expect(report.state.enumCursor.complete).toBe(false);
    // And no debt was enqueued to be worked later against a list we never read —
    // the pending set is what a "successful" empty list would have left empty and
    // then declared finished.
    expect(report.state.pending).toEqual([]);
  });

  it('the ledger keeps the refusal, and the next run does not re-ask it (permanent, not a backoff)', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const be = backend(clock, { [DEEPSEEK_LIST_PATH]: LIST_REFUSAL });

    const first = await run(store, be.http, 'default');
    expect(first.halted?.reason).toBe('auth-refused');

    // 🔴 Permanent by haltClassOf's default, and deliberately NOT 'rate-limited':
    //    a transient reason would be retried on a backoff ladder and the popup
    //    would promise a resume that a missing token cannot deliver.
    const second = await run(store, be.http, 'default');
    expect(second.stopped).toBe('halted');
    expect(second.halted?.reason).toBe('auth-refused');
    // Refused from the stored record: the second run sent nothing at all.
    expect(be.calls).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// 2 · Reading the code: what is a refusal, what is not
// ---------------------------------------------------------------------------

describe('W61-2 · deepSeekEnvelopeRefusal reads the code the parsers used to discard', () => {
  it('refuses the two codes the probe measured, and names both code and message', () => {
    const list = deepSeekEnvelopeRefusal(LIST_REFUSAL);
    expect(list?.reason).toBe('auth-refused');
    expect(list?.detail).toContain('code 40002');
    expect(list?.detail).toContain('Missing Token');

    const detail = deepSeekEnvelopeRefusal(DETAIL_REFUSAL);
    expect(detail?.reason).toBe('auth-refused');
    expect(detail?.detail).toContain('code 40003');
    expect(detail?.detail).toContain('INVALID_TOKEN');

    // The nested business code is read too: a refusal that only sets
    // `data.biz_code` must not be missed, since `data` is exactly the object a
    // top-level refusal nulls out.
    const nested = deepSeekEnvelopeRefusal(JSON.stringify({
      code: 0,
      data: { biz_code: 40003, biz_msg: 'INVALID_TOKEN' },
    }));
    expect(nested?.reason).toBe('auth-refused');
    expect(nested?.detail).toContain('data.biz_code 40003');
  });

  it('says nothing about a well-formed envelope — the measured success shape', () => {
    // 🔴 W57 §9 recorded our row for `code` / `biz_code` as "never read". Reading
    //    them must not turn a good answer into a stop: this is the shape the probe
    //    measured with a bearer, and it must pass through untouched.
    expect(deepSeekEnvelopeRefusal(goodListPage([ID]))).toBeNull();
    expect(deepSeekEnvelopeRefusal(goodDetailPage())).toBeNull();
    // A zero code in either position, present and explicit.
    expect(deepSeekEnvelopeRefusal(JSON.stringify({ code: 0, data: { biz_code: 0 } }))).toBeNull();
  });

  it('leaves anything it cannot read to the shape checks, rather than calling it a refusal', () => {
    // Not a refusal, and not silently "success" either — these are the shape
    // checks' business, and reading a non-number as agreement would be inventing
    // a fact the platform did not state.
    expect(deepSeekEnvelopeRefusal('not json')).toBeNull();
    expect(deepSeekEnvelopeRefusal('[]')).toBeNull();
    expect(deepSeekEnvelopeRefusal('null')).toBeNull();
    expect(deepSeekEnvelopeRefusal(JSON.stringify({ code: '0' }))).toBeNull();
    expect(deepSeekEnvelopeRefusal(JSON.stringify({ code: null }))).toBeNull();
    expect(deepSeekEnvelopeRefusal(JSON.stringify({ msg: 'no code at all' }))).toBeNull();
  });

  it('bounds and sanitises the platform\'s own message before it reaches the ledger', () => {
    const noisy = deepSeekEnvelopeRefusal(JSON.stringify({
      code: 40002,
      msg: `line one\nline two\u0000${'x'.repeat(500)}`,
    }));
    expect(noisy?.detail).not.toContain('\n');
    expect(noisy?.detail).not.toContain('\u0000');
    expect(noisy?.detail.length).toBeLessThan(400);
    // A code with no message still reads as a refusal.
    const bare = deepSeekEnvelopeRefusal(JSON.stringify({ code: 40002 }));
    expect(bare?.detail).toContain('code 40002');
    expect(bare?.detail).not.toContain('"');
  });

  it('the list parser is unchanged: the refusal never reaches it, so it still refuses a body it cannot read', () => {
    // 🔴 The refusal is an envelope-level fact the engine handles before the parser
    //    (see the note on BackfillEnumPlan.refusalOf). What must NOT happen is the
    //    parser getting looser: a body with no readable `chat_sessions` is still a
    //    shape refusal, and a good page still parses.
    const drift = parseDeepSeekListPage(JSON.stringify({ code: 0, data: { biz_data: {} } }));
    expect(drift.ok).toBe(false);
    const good = parseDeepSeekListPage(goodListPage([ID]));
    expect(good.ok).toBe(true);
    if (good.ok) expect(good.page.ids).toEqual([ID]);
  });
});

// ---------------------------------------------------------------------------
// 3 · The bearer: where it goes, what it is, and what happens without it
// ---------------------------------------------------------------------------

describe('W61-3 · the bearer token goes to two paths and nowhere else', () => {
  it('needsDeepSeekBearer is true for exactly the two backfill paths on a DeepSeek origin', () => {
    expect(needsDeepSeekBearer(LIST_URL, ORIGIN)).toBe(true);
    expect(needsDeepSeekBearer(DETAIL_URL, ORIGIN)).toBe(true);

    // Lookalikes and neighbours, none of which is this.
    expect(needsDeepSeekBearer(`${ORIGIN}${DEEPSEEK_LIST_PATH}/extra`, ORIGIN)).toBe(false);
    expect(needsDeepSeekBearer(`${ORIGIN}${DEEPSEEK_LIST_PATH}`, `${ORIGIN}/app`)).toBe(false);
    expect(needsDeepSeekBearer(`${ORIGIN}/api/v0/chat/completion`, ORIGIN)).toBe(false);
    expect(needsDeepSeekBearer(`${ORIGIN}/api/v0/chat_session/fetch_page_backup`, ORIGIN)).toBe(false);
    expect(needsDeepSeekBearer('https://chat.deepseek.com.attacker.example/api/v0/chat_session/fetch_page', ORIGIN)).toBe(false);
    expect(needsDeepSeekBearer('not a url', ORIGIN)).toBe(false);
  });

  it('attaches the token to the two paths, and leaves every other request byte for byte', async () => {
    const calls: { url: string; headers: Record<string, string> }[] = [];
    const rawFetch = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
      calls.push({ url, headers: { ...(init.headers as Record<string, string> | undefined) } });
      return { status: 200, text: async () => '{}' };
    };
    const fetchWithToken = createDeepSeekAuthorizedFetch(ORIGIN, rawFetch, { readToken: () => TOKEN });

    await fetchWithToken(LIST_URL, { headers: { accept: 'application/json' } });
    await fetchWithToken(DETAIL_URL, { headers: { accept: 'application/json' } });
    await fetchWithToken(`${ORIGIN}/api/v0/chat/completion`, { headers: { accept: 'application/json' } });

    expect(calls[0]!.headers.authorization).toBe(`Bearer ${TOKEN}`);
    expect(calls[1]!.headers.authorization).toBe(`Bearer ${TOKEN}`);
    // 🔴 A third path on the same origin: not this leg's request, not our header.
    expect(calls[2]!.headers.authorization).toBeUndefined();
    // The caller's own headers survive.
    expect(calls[0]!.headers.accept).toBe('application/json');
    // Kimi's two headers are not DeepSeek's, and must not ride along.
    expect(calls[0]!.headers['x-msh-platform']).toBeUndefined();
    expect(calls[0]!.headers['x-language']).toBeUndefined();
  });

  it('sends no authorization header at all when there is no token, so the platform\'s own refusal is what the leg sees', async () => {
    const calls: { url: string; headers: Record<string, string> }[] = [];
    const rawFetch = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
      calls.push({ url, headers: { ...(init.headers as Record<string, string> | undefined) } });
      return { status: 200, text: async () => LIST_REFUSAL };
    };
    const fetchWithToken = createDeepSeekAuthorizedFetch(ORIGIN, rawFetch, { readToken: () => null });

    const res = await fetchWithToken(LIST_URL, {});

    // 🔴 Not `Bearer ` with an empty value: an absent header is a different request
    //    from an empty one, and the platform has not been asked what it makes of the
    //    latter. The mock's body is the real refusal, so downstream reads 40002.
    expect(calls).toHaveLength(1);
    expect(calls[0]!.headers.authorization).toBeUndefined();
    expect(res.status).toBe(200);
    expect(deepSeekEnvelopeRefusal(await res.text())?.reason).toBe('auth-refused');
  });

  it('re-reads the token once after a 401 or 403, and not at all when it sent none', async () => {
    const readToken = vi.fn().mockReturnValue(TOKEN);
    const statuses: number[] = [];
    const rawFetch = async (): Promise<MinimalResponse> => {
      const status = statuses.length === 0 ? 401 : 200;
      statuses.push(status);
      return { status, text: async () => '{}' };
    };
    const fetchWithToken = createDeepSeekAuthorizedFetch(ORIGIN, rawFetch, { readToken });
    expect((await fetchWithToken(LIST_URL, {})).status).toBe(200);
    expect(readToken).toHaveBeenCalledTimes(2);
    expect(statuses).toEqual([401, 200]);

    // A 403 is the other half of the family the ticket names.
    const forbidden: number[] = [];
    const readToken403 = vi.fn().mockReturnValue(TOKEN);
    const fetch403 = createDeepSeekAuthorizedFetch(ORIGIN, async () => {
      const status = forbidden.length === 0 ? 403 : 200;
      forbidden.push(status);
      return { status, text: async () => '{}' };
    }, { readToken: readToken403 });
    expect((await fetch403(LIST_URL, {})).status).toBe(200);
    expect(forbidden).toEqual([403, 200]);

    // 🔴 With no token there is nothing to re-read: the retry would be
    //    byte-identical, so it is not sent. Two requests for a logged-out user,
    //    both reaching the same answer, is the cost this avoids.
    const loggedOut = vi.fn().mockReturnValue(null);
    let calls = 0;
    const fetchNoToken = createDeepSeekAuthorizedFetch(ORIGIN, async () => {
      calls += 1;
      return { status: 401, text: async () => '{}' };
    }, { readToken: loggedOut });
    expect((await fetchNoToken(LIST_URL, {})).status).toBe(401);
    expect(calls).toBe(1);
    expect(loggedOut).toHaveBeenCalledTimes(1);
  });

  it('a 200 is never retried — the refusal this platform actually sends is in the body', async () => {
    const readToken = vi.fn().mockReturnValue(TOKEN);
    let calls = 0;
    const fetchWithToken = createDeepSeekAuthorizedFetch(ORIGIN, async () => {
      calls += 1;
      return { status: 200, text: async () => LIST_REFUSAL };
    }, { readToken });
    const res = await fetchWithToken(LIST_URL, {});
    expect(res.status).toBe(200);
    // 🔴 One request, and the wrapper does not try to interpret the body: re-reading
    //    the token on a 200 would send byte-identical bytes, because the token is
    //    read per request already. The honest reason for this body is the engine's
    //    job, and it is `auth-refused`.
    expect(calls).toBe(1);
    expect(readToken).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// 4 · What the page actually stores
// ---------------------------------------------------------------------------

describe('W61-4 · userToken is a JSON object, and the token is its .value', () => {
  it('unwraps the measured shape and passes a bare token through', () => {
    // The measured storage value is JSON around the token (92 characters in the
    // probed profile); two of the W57 references read `.value` and one does not.
    expect(readDeepSeekUserToken(JSON.stringify({ value: TOKEN }))).toBe(TOKEN);
    // A platform that stops wrapping its token in JSON must not thereby lose it.
    expect(readDeepSeekUserToken(TOKEN)).toBe(TOKEN);
  });

  it('returns null for every way a token can be missing, unreadable or unusable', () => {
    // 🔴 Four different facts, one answer — and the answer is "send no header",
    //    never "send an empty one". The platform's own refusal is then the record.
    expect(readDeepSeekUserToken(null)).toBeNull();
    expect(readDeepSeekUserToken('')).toBeNull();
    expect(readDeepSeekUserToken('{}')).toBeNull();
    expect(readDeepSeekUserToken(JSON.stringify({ value: '' }))).toBeNull();
    expect(readDeepSeekUserToken(JSON.stringify({ value: 12345 }))).toBeNull();
    expect(readDeepSeekUserToken('[]')).toBeNull();
    expect(readDeepSeekUserToken('{"value":')).toBeNull();
    // A control character cannot go into an HTTP header at all: `fetch` would throw
    // and surface as a transport error about a value we could see was unusable.
    expect(readDeepSeekUserToken(JSON.stringify({ value: 'bad\nvalue' }))).toBeNull();
    expect(readDeepSeekUserToken(JSON.stringify({ value: 'bad\u0000value' }))).toBeNull();
  });

  it('the storage key is the one the page uses, and it is the only thing named here', () => {
    expect(DEEPSEEK_USER_TOKEN_STORAGE_KEY).toBe('userToken');
  });
});

// ---------------------------------------------------------------------------
// 5 · Nothing else moved
// ---------------------------------------------------------------------------

describe('W61-5 · the change adds a stop and loosens nothing', () => {
  it('a well-formed body still goes to the sink, with the hook declared on the plan', async () => {
    const store = memoryStore();
    const clock = fakeClock();
    const be = backend(clock, {
      [DEEPSEEK_LIST_PATH]: goodListPage([ID]),
      [DEEPSEEK_DETAIL_PATH]: goodDetailPage(),
    });

    const report = await run(store, be.http, 'default');

    // The refusal hook answered null and every existing check ran as before: the
    // list parsed, the debt was enqueued, the body passed the shape gate and W42's
    // tree walk, and it was archived.
    expect(report.stopped).not.toBe('halted');
    expect(report.halted).toBeNull();
    expect(report.archivedThisRun).toEqual([ID]);
  });

  it('the shape gate is not relaxed for a refusal — it is simply not reached', () => {
    // 🔴 Stated as a property rather than assumed: the contract row still refuses a
    //    refusal envelope on its own terms, which is exactly why the engine has to
    //    ask the plan first. If this ever became true, the plan hook would be
    //    redundant — and if it silently stopped being checked, the hook would be
    //    the only thing standing between a refusal and a wrong reason.
    //
    //    The row describes the **conversation body**, not the list: its
    //    requiredAnyPaths name `data.biz_data.chat_messages` and friends, and no
    //    entry is `chat_sessions`. That is why the engine applies this gate to the
    //    delivered body only, and it is the reason the body segment needs the plan
    //    hook while the list segment does not.
    const row = PLATFORMS.find((platform) => platform.id === 'deepseek')!;
    expect(matchesResponseShape(row, DETAIL_REFUSAL)).toBe(false);
    expect(matchesResponseShape(row, goodDetailPage())).toBe(true);
    // The refusal envelope really does fail the row for the reason claimed — no
    // `data` at all — rather than by accident of some other field.
    expect(JSON.parse(DETAIL_REFUSAL).data).toBeNull();
  });

  it('the plan declares the refusal, and only DeepSeek does', () => {
    expect(DEEPSEEK_PLAN.refusalOf).toBe(deepSeekEnvelopeRefusal);
    // The hook is DeepSeek's measured fact, not a general one: every other plan
    // leaves it undeclared, so their 2xx bodies take exactly the path they did.
    for (const plan of [CHATGPT_PLAN, PERPLEXITY_PLAN, GROK_PLAN, KIMI_PLAN, GEMINI_PLAN, CLAUDE_PLAN]) {
      expect(plan.refusalOf).toBeUndefined();
    }
  });

  it('the popup names the reason in its own sentence, not the other fallback', () => {
    // 🔴 The repo's own rule (W45): a named reason whose wording is the `other`
    //    fallback's is not really named. This one must also not borrow
    //    `waitingRetry`, which promises a self-resuming wait.
    const sentence = t('popup.notes.halted.authRefused', { platform: 'deepseek', detail: 'code 40002' });
    expect(sentence).toContain('deepseek');
    expect(sentence).toContain('code 40002');
    expect(sentence).not.toBe(t('popup.notes.halted.other', { reason: 'auth-refused', detail: 'code 40002' }));
    expect(sentence).not.toBe(t('popup.notes.halted.waitingRetry', {
      reason: 'auth-refused', attempts: 1, minutes: 5, detail: 'code 40002',
    }));
    // It says the two things a user needs and the generic sentence does not.
    expect(sentence.toLowerCase()).toContain('sign in');
  });

  it('the DeepSeek origin list is still closed', () => {
    const row = PLATFORMS.find((platform) => platform.id === 'deepseek')!;
    expect(row.origins).toEqual(['https://chat.deepseek.com']);
    expect(PAGE_URL.startsWith(row.origins[0]!)).toBe(true);
  });
});
