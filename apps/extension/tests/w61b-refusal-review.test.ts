/**
 * W61b · **Four findings from the adversarial review of `264ea85`.**
 *
 * `264ea85` fixed a real defect — DeepSeek refuses in-band, so a login refusal was
 * being recorded as a wire-shape change — and introduced four of its own. This
 * file is the regression nail for each, and every one of these assertions was run
 * against `264ea85` before the fix and shown failing there (the red run is in the
 * W61b report; the assertions name the old behaviour in the message only, never by
 * weakening what they demand of the new one).
 *
 *  1. **`auth-refused` was the answer to every non-zero code.** The predicate was
 *     `typeof code === 'number' && code !== 0`, so `"server busy"` and `"too many
 *     requests"` were recorded as "you are not logged in" and never retried — the
 *     mirror of the bug W61 was fixing. Only a code that was **measured** to mean
 *     a credential failure may be `auth-refused`; a code/message that names a rate
 *     or busy condition is transient; a code this build cannot read is said to be
 *     unreadable, never guessed.
 *  2. **`auth-refused` was permanent**, and nothing in the product clears a
 *     permanent record — so a temporary logout froze DeepSeek's backfill across
 *     logins and across updates, while the token is re-read on every request. The
 *     leg must come back by itself, gently, and recover the moment the user signs
 *     back in, with nobody touching storage.
 *  3. **The popup sentence was false for the body segment.** It said "stopped
 *     without reading anything" and "the list request itself was the one refused"
 *     — while the list had already been read and its ids were sitting in `pending`.
 *     A sentence that is only true for one of the two segments it is printed for
 *     is the same defect class as recording an unknown as empty.
 *  4. **Token hygiene.** The bearer rode along on a redirect, and a leading space
 *     or BOM before the JSON gate made the wrapper send the whole stored string
 *     (` {"value":"<token>"}`) as the token instead of nothing.
 *
 * 🔴 All fixtures are synthetic, written from the key names the probe measured
 *    (2026-09-23). No request goes to chat.deepseek.com, there is no logged-in
 *    state, and no real conversation, account, token or id appears anywhere below.
 */

import { describe, expect, it } from 'vitest';
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { headerOf, haltClassOf, isTransientReason } from '../lib/backfill/types';
import { BACKFILL_TICK_DELAY_MIN_MINUTES } from '../lib/backfill/alarm';
import {
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_LIST_PATH,
  deepSeekEnvelopeRefusal,
} from '../lib/backfill/enumerate';
import {
  createDeepSeekAuthorizedFetch,
  readDeepSeekUserToken,
  type MinimalResponse,
} from '../lib/platform-auth';
import { NO_FAILURES, renderPopup, type PopupModel } from '../lib/popup-view';
import { t } from '../lib/i18n';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chat.deepseek.com';
const LIST_URL = `${ORIGIN}${DEEPSEEK_LIST_PATH}?count=100`;
const ID = 'b7f1c2d3-e4a5-4b6c-8d7e-9f0a1b2c3d4e';
const DETAIL_URL = `${ORIGIN}${DEEPSEEK_DETAIL_PATH}?chat_session_id=${encodeURIComponent(ID)}`;
/** A synthetic token, never the real thing. */
const TOKEN = 'synthetic.deepseek.token';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};
const T0 = Date.parse('2026-09-23T00:00:00.000Z');

/** A clock whose instant the test moves, so "not yet due" and "due now" are decided, not sampled. */
function movableClock(start: number): Clock & { at: (t: number) => void } {
  let now = start;
  return {
    now: () => now,
    async sleep(ms: number) {
      now += ms;
    },
    at(t: number) {
      now = t;
    },
  };
}

function session(id: string, seqId: number): Record<string, unknown> {
  return { id, seq_id: seqId, updated_at: 1758600000000, title: 'synthetic title', current_message_id: 2 };
}

function goodListPage(ids: string[]): string {
  return JSON.stringify({
    code: 0,
    msg: '',
    data: {
      biz_code: 0,
      biz_msg: '',
      biz_data: { chat_sessions: ids.map((id, i) => session(id, ids.length - i)), has_more: false },
    },
  });
}

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
          { message_id: 2, parent_id: 1, role: 'ASSISTANT', status: 'FINISHED', inserted_at: 1758501000000 },
        ],
        chat_session: { id: ID, current_message_id: 2 },
      },
    },
  });
}

/** The two refusals the probe measured — both credential failures. */
const LIST_AUTH_REFUSAL = JSON.stringify({ code: 40002, data: null, msg: 'Missing Token' });
const BODY_AUTH_REFUSAL = JSON.stringify({ code: 40003, data: null, msg: 'INVALID_TOKEN' });

/** A **routing backend**: a mutable path → body map, so a leg's answer can change between rounds. */
function backend(clock: Clock) {
  const calls: { url: string; at: number }[] = [];
  const routes = new Map<string, string>();
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push({ url, at: clock.now() });
    const path = new URL(url).pathname;
    const route = routes.get(path);
    if (route === undefined) throw new Error(`unexpected path ${path}`);
    return { status: 200, text: route };
  };
  return { calls, routes, http };
}

function opts(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  clock: Clock,
  scope: string,
): Parameters<typeof runBackfill>[0] {
  return {
    platform: 'deepseek',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock,
    pace: NO_WAIT,
    sink: (captured): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
  };
}

/** One DeepSeek run against the shared backend, at a named scope. */
function run(
  store: ReturnType<typeof memoryStore>,
  be: ReturnType<typeof backend>,
  clock: Clock,
  scope: string,
) {
  return runBackfill(opts(store, be.http, clock, scope));
}

// ---------------------------------------------------------------------------
// 1 · Which codes may be called an auth refusal
// ---------------------------------------------------------------------------

describe('W61b-1 · only a code that means auth is auth-refused', () => {
  it('🔴 "server busy" is not "you are not logged in" — it is the rate family, and it is transient', () => {
    // The review's own example: a 200 whose envelope says the platform is busy.
    // `264ea85` recorded this as `auth-refused` with a popup naming the login.
    const busy = deepSeekEnvelopeRefusal(JSON.stringify({ code: 1, msg: 'too many requests', data: null }));
    expect(busy?.reason as string).toBe('rate-limited');
    expect(busy?.detail).toContain('code 1');
    expect(busy?.detail).toContain('too many requests');
    expect(haltClassOf(busy!.reason)).toBe('transient');
    expect(isTransientReason(busy!.reason)).toBe(true);

    // The nested position, which the review names separately: this is the shape the
    // envelope uses when the top-level code is a clean 0.
    const nested = deepSeekEnvelopeRefusal(JSON.stringify({
      code: 0,
      data: { biz_code: 1, biz_msg: 'server busy', biz_data: null },
    }));
    expect(nested?.reason as string).toBe('rate-limited');
    expect(nested?.detail).toContain('data.biz_code 1');
    expect(nested?.detail).toContain('server busy');
  });

  it('🔴 a code this build cannot read stays unreadable — it is never guessed as auth', () => {
    const strange = deepSeekEnvelopeRefusal(JSON.stringify({
      code: 7,
      msg: 'a code no source mentions',
      data: null,
    }));
    expect(strange?.reason as string).toBe('refused-unknown');
    // The platform's own words ride along, so a code that turns out to mean
    // something else is readable in the trace rather than rounded into a sentence
    // about logging in.
    expect(strange?.detail).toContain('code 7');
    expect(strange?.detail).toContain('a code no source mentions');
    expect(strange?.reason).not.toBe('auth-refused');

    // 🔴 Neighbours of the two measured codes are not assumed to be credentials
    //    because they sit next to them numerically. The auth set is evidence.
    for (const code of [40001, 40004, 40005, 400]) {
      const refusal = deepSeekEnvelopeRefusal(JSON.stringify({ code, msg: '', data: null }));
      expect(refusal?.reason as string, `code ${code}`).toBe('refused-unknown');
    }

    // 🔴 A **message** that mentions a token does not promote an unmeasured code to
    //    an auth refusal: the code is the platform's own claim about the kind of
    //    failure, and the message is free text. This is the conservative side, and
    //    it is stated because it has a cost — a future credential code that was
    //    never measured is reported as unreadable rather than as "log in".
    const tokenish = deepSeekEnvelopeRefusal(JSON.stringify({ code: 1, msg: 'Missing Token', data: null }));
    expect(tokenish?.reason as string).toBe('refused-unknown');
  });

  it('the two measured credential codes are still auth, in both positions', () => {
    const list = deepSeekEnvelopeRefusal(LIST_AUTH_REFUSAL);
    expect(list?.reason as string).toBe('auth-refused');
    expect(list?.detail).toContain('code 40002');
    const body = deepSeekEnvelopeRefusal(BODY_AUTH_REFUSAL);
    expect(body?.reason as string).toBe('auth-refused');
    expect(body?.detail).toContain('code 40003');
    // The nested position too: `data.biz_code` is a sibling of the payload a
    // refusal nulls out, so it is the position a refusal-on-the-payload uses.
    const nested = deepSeekEnvelopeRefusal(JSON.stringify({
      code: 0,
      data: { biz_code: 40003, biz_msg: 'INVALID_TOKEN' },
    }));
    expect(nested?.reason as string).toBe('auth-refused');
    expect(nested?.detail).toContain('data.biz_code 40003');
    // Well-formed answers are still answered with silence, and unreadable code
    // types are still left to the shape checks rather than read as agreement.
    expect(deepSeekEnvelopeRefusal(goodListPage([ID]))).toBeNull();
    expect(deepSeekEnvelopeRefusal(JSON.stringify({ code: '0' }))).toBeNull();
    expect(deepSeekEnvelopeRefusal(JSON.stringify({ code: 0, data: { biz_code: 0 } }))).toBeNull();
  });

  it('the round trip through the engine: a busy envelope is a waiting leg, not "not logged in"', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, JSON.stringify({ code: 1, msg: 'too many requests', data: null }));

    const report = await run(store, be, clock, 'w61b-busy');

    // 🔴 `264ea85` halted here — a human had to look, the popup said the token was
    //    missing, and the leg never came back. A rate limit is the one thing the
    //    backoff ladder exists for.
    expect(report.stopped).toBe('waiting-retry');
    expect(report.halted?.reason as string).toBe('rate-limited');
    expect(report.halted?.retryAt).toBeGreaterThan(clock.now());
    expect(report.halted?.detail).toContain('too many requests');
  });

  it('the round trip through the engine: an unreadable code is named as unreadable', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, JSON.stringify({ code: 7, msg: 'synthetic', data: null }));

    const report = await run(store, be, clock, 'w61b-unknown');

    expect(report.halted?.reason as string).toBe('refused-unknown');
    // Not a shape change (the wire is the same envelope it always was) and not an
    // auth refusal (nothing was measured about this code) — and it does not
    // pretend to know what waiting means for it either: it is the gentle-retry
    // family, so the leg asks again rather than standing until a human looks.
    expect(report.halted?.reason).not.toBe('shape-changed');
    expect(report.halted?.reason).not.toBe('auth-refused');
    expect(isTransientReason(report.halted!.reason)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 2 · A temporary logout must not be a permanent stop
// ---------------------------------------------------------------------------

describe('W61b-2 · an auth refusal comes back on its own', () => {
  it('🔴 it is transient, it is not permanent, and a retry moment is written down', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, LIST_AUTH_REFUSAL);

    const report = await run(store, be, clock, 'w61b-auth-1');

    // 🔴 `264ea85` made this 'permanent' and wrote no `retryAt`: nothing in the
    //    product clears a permanent record, so the leg never asked again.
    expect(haltClassOf(report.halted!.reason)).toBe('transient');
    expect(report.stopped).toBe('waiting-retry');
    const retryAt = report.halted?.retryAt;
    expect(typeof retryAt).toBe('number');
    // Gently: at least one whole alarm tick away, so this is a backoff and not a
    // hammer. The exact rung is tuning and is not pinned here; what is pinned is
    // that a rung exists and that it is finite (the leg is not effectively stopped).
    expect(retryAt! - clock.now()).toBeGreaterThanOrEqual(BACKFILL_TICK_DELAY_MIN_MINUTES * 60_000);
    expect(retryAt! - clock.now()).toBeLessThan(24 * 3600_000);
  });

  it('🔴 while it waits it sends nothing; when the wait is over it asks again', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, LIST_AUTH_REFUSAL);

    const first = await run(store, be, clock, 'w61b-auth-2');
    expect(first.halted?.reason).toBe('auth-refused');
    expect(be.calls).toHaveLength(1);

    // Not yet due: a waiting round is free — no request, and the same record back.
    clock.at(first.halted!.retryAt! - 1);
    const waiting = await run(store, be, clock, 'w61b-auth-2');
    expect(waiting.stopped).toBe('waiting-retry');
    expect(be.calls).toHaveLength(1);

    // Due: the leg re-asks on its own. The streak continues across the resume, so
    // the ladder does not restart from its first rung.
    clock.at(first.halted!.retryAt!);
    const second = await run(store, be, clock, 'w61b-auth-2');
    expect(be.calls).toHaveLength(2);
    expect(second.halted?.reason).toBe('auth-refused');
    expect(second.halted?.attempts).toBe(2);
    expect(second.halted?.retryAt).toBeGreaterThan(clock.now());
  });

  it('🔴 signing back in is enough: the very next due round reads the list, with no storage touched', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, LIST_AUTH_REFUSAL);
    be.routes.set(DEEPSEEK_DETAIL_PATH, goodDetailPage());

    const refused = await run(store, be, clock, 'w61b-auth-3');
    expect(refused.halted?.reason).toBe('auth-refused');

    // The user opens DeepSeek in this browser and signs in. Nothing else changes:
    // no popup click, no storage edit, no new scope. The token is re-read per
    // request, so the next round that is allowed to ask simply succeeds.
    be.routes.set(DEEPSEEK_LIST_PATH, goodListPage([ID]));
    clock.at(refused.halted!.retryAt!);
    const recovered = await run(store, be, clock, 'w61b-auth-3');

    expect(recovered.halted).toBeNull();
    expect(recovered.archivedThisRun).toEqual([ID]);
    expect(be.calls.map((c) => new URL(c.url).pathname)).toEqual([
      DEEPSEEK_LIST_PATH,
      DEEPSEEK_LIST_PATH,
      DEEPSEEK_DETAIL_PATH,
    ]);
  });
});

// ---------------------------------------------------------------------------
// 3 · The sentence is true for both segments it is printed for
// ---------------------------------------------------------------------------

/** The popup model for a persisted state, through the same front door the popup uses. */
function modelFor(state: Parameters<typeof headerOf>[0]): PopupModel {
  return {
    enabled: true,
    block: null,
    state: headerOf(state),
    target: { platform: 'deepseek', scope: 'default' },
    failures: NO_FAILURES,
  };
}

describe('W61b-3 · the auth-refused sentence is not a claim about the list segment', () => {
  it('🔴 a body refusal renders a sentence that is still true when ids are already pending', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, goodListPage([ID]));
    be.routes.set(DEEPSEEK_DETAIL_PATH, BODY_AUTH_REFUSAL);

    const report = await run(store, be, clock, 'w61b-sentence');
    expect(report.halted?.reason).toBe('auth-refused');
    // 🔴 **This is the state the old sentence lied about**: the list really was
    //    read, and the id it carried is sitting in the debt set right now.
    expect(report.state.pending.length).toBeGreaterThan(0);

    const notes = renderPopup(modelFor(report.state)).notes.join('\n');

    // The stop is named, and the platform's own code reaches the screen.
    expect(notes).toContain('code 40003');
    // 🔴 The two clauses the review names. Both were false here — nothing was
    //    refused on the list, and a conversation had already been listed — and a
    //    sentence that is only true for the other segment is the defect.
    expect(notes).not.toContain('the list request itself was the one refused');
    expect(notes).not.toContain('stopped without reading anything');
    expect(notes).not.toContain('nothing about your history has been read yet');
    // It still says the two things a user needs: why, and what to do.
    expect(notes.toLowerCase()).toContain('sign in');
    expect(notes).toContain('stays exactly as it was');
  });

  it('and the same sentence is true for the list segment, where nothing was read', async () => {
    const store = memoryStore();
    const clock = movableClock(T0);
    const be = backend(clock);
    be.routes.set(DEEPSEEK_LIST_PATH, LIST_AUTH_REFUSAL);

    const report = await run(store, be, clock, 'w61b-sentence-list');
    const notes = renderPopup(modelFor(report.state)).notes.join('\n');

    expect(report.halted?.reason).toBe('auth-refused');
    expect(notes).toContain('code 40002');
    // The sentence is one sentence for both segments — not a segment guess — so
    // what it must be is true in both. Here: nothing was listed and nothing was
    // written off, which is what the run itself says.
    expect(notes).toContain('stays exactly as it was');
    expect(report.state.pending).toEqual([]);
  });

  it('the sentence is the reason\'s own, and it says the leg comes back by itself', () => {
    const sentence = t('popup.notes.halted.authRefused', {
      platform: 'deepseek',
      detail: 'code 40002',
      minutes: 30,
      attempts: 1,
    });
    // 🔴 The W45 rule: a named reason whose wording is the `other` fallback's is
    //    not really named.
    expect(sentence).not.toBe(t('popup.notes.halted.other', { reason: 'auth-refused', detail: 'code 40002' }));
    // 🔴 And the half W61b-2 added, without which the sentence would contradict
    //    the record: the leg is not waiting for a human to clear storage, it is
    //    coming back on its own — the login is what makes the next round work.
    expect(sentence).toContain('30');
    expect(sentence.toLowerCase()).toContain('sign in');
    // The unknown-code family has a sentence of its own too, so it cannot borrow
    // either of the two sentences above and claim to know what the code means.
    const unknown = t('popup.notes.halted.refusedUnknown', {
      platform: 'deepseek',
      detail: 'code 7',
      minutes: 30,
      attempts: 1,
    });
    expect(unknown).not.toBe(t('popup.notes.halted.other', { reason: 'refused-unknown', detail: 'code 7' }));
    expect(unknown).toContain('7');
  });
});

// ---------------------------------------------------------------------------
// 4 · Token hygiene
// ---------------------------------------------------------------------------

describe('W61b-4 · the bearer is never re-sent by a redirect, and never sent half-parsed', () => {
  it('🔴 a leading space or BOM never makes the stored string itself the token', () => {
    const stored = JSON.stringify({ value: TOKEN });
    // The stored value is JSON; a leading space or BOM is storage's, not the
    // token's, and `264ea85` sent the whole string as the bearer
    // (`Authorization: Bearer  {"value":"…"}`).
    expect(readDeepSeekUserToken(` ${stored}`)).toBe(TOKEN);
    expect(readDeepSeekUserToken(`﻿${stored}`)).toBe(TOKEN);
    expect(readDeepSeekUserToken(`\t\n ${stored} `)).toBe(TOKEN);
    // Whitespace-only, and a space in front of something that is neither JSON nor
    // a token, are the same answer as an empty storage: nothing to send.
    expect(readDeepSeekUserToken(' ')).toBeNull();
    expect(readDeepSeekUserToken('﻿')).toBeNull();
    expect(readDeepSeekUserToken(' {"value":')).toBeNull();
    // A bare token still passes through, and its own surrounding whitespace is
    // not part of it either.
    expect(readDeepSeekUserToken(TOKEN)).toBe(TOKEN);
    expect(readDeepSeekUserToken(` ${TOKEN} `)).toBe(TOKEN);
  });

  it('🔴 DeepSeek\'s two paths are fetched with redirect: manual, and other paths are untouched', async () => {
    const calls: { url: string; init: RequestInit }[] = [];
    const rawFetch = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
      calls.push({ url, init });
      return { status: 200, text: async () => '{}' };
    };
    const fetchWithToken = createDeepSeekAuthorizedFetch(ORIGIN, rawFetch, { readToken: () => TOKEN });
    const other = { headers: { accept: 'application/json' } } as RequestInit;

    await fetchWithToken(LIST_URL, { headers: { accept: 'application/json' } });
    await fetchWithToken(DETAIL_URL, { headers: { accept: 'application/json' } });
    await fetchWithToken(`${ORIGIN}/api/v0/chat/completion`, other);

    // 🔴 `264ea85` left `redirect` at the browser's default (`follow`), so a 302
    //    from either endpoint re-sent the page's own bearer to wherever it pointed.
    expect(calls[0]!.init.redirect).toBe('manual');
    expect(calls[1]!.init.redirect).toBe('manual');
    // A path this wrapper does not own is not touched at all — not even to add a
    // redirect policy: the caller's own init object is passed through.
    expect(calls[2]!.init).toBe(other);
    expect(calls[2]!.init.redirect).toBeUndefined();
    // The token itself still rides on the one request that was sent.
    expect((calls[0]!.init.headers as Record<string, string>).authorization).toBe(`Bearer ${TOKEN}`);
  });

  it('🔴 an unfollowed redirect is a failure with a trace, not a response that gets read', async () => {
    let calls = 0;
    const rawFetch = async (): Promise<MinimalResponse> => {
      calls += 1;
      // What a `redirect: 'manual'` fetch resolves to when the platform redirects:
      // an opaque response — status 0, no readable body.
      return { status: 0, text: async () => '' };
    };
    const fetchWithToken = createDeepSeekAuthorizedFetch(ORIGIN, rawFetch, { readToken: () => TOKEN });

    // 🔴 `264ea85` handed this straight back, the engine read `HTTP 0` as a wire
    //    shape change, and the leg stopped for good. It is a transport fact: said
    //    out loud, and retried like one.
    await expect(fetchWithToken(LIST_URL, {})).rejects.toThrow(/redirect/i);
    // Not retried: re-reading the token cannot change an answer that was never read.
    expect(calls).toBe(1);

    // A 200 is still a 200 — the guard is exactly the unfollowed-redirect case.
    const ok = createDeepSeekAuthorizedFetch(ORIGIN, async () => ({
      status: 200,
      text: async () => LIST_AUTH_REFUSAL,
    }), { readToken: () => TOKEN });
    expect((await ok(LIST_URL, {})).status).toBe(200);
  });

  it('the reason the guard exists: the token is the page\'s, and the redirect is not ours to trust', () => {
    // 🔴 Stated as the property the two assertions above are about: same-origin
    //    redirects keep the `Authorization` header, and the wrapper has no idea
    //    where the endpoint may point today. The only safe answer with a page's own
    //    bearer in the request is not to follow.
    expect(new URL(LIST_URL).origin).toBe(ORIGIN);
    expect(new URL(`${ORIGIN}${DEEPSEEK_LIST_PATH}`).pathname).toBe(DEEPSEEK_LIST_PATH);
  });
});
