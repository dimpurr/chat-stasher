/**
 * W308 · Gemini's `batchexecute` refusals and the shape of a halt.
 *
 * ## The defect this file exists to stop happening again
 * A Gemini backfill enumerated 520 conversations and then stopped. The record said
 * `shape-changed` with the detail `gemini list response could not be read:
 * payload-unusable` — one word for three different facts:
 *
 *   1. the matched `wrb.fr` entry carried **no document** at `[2]`;
 *   2. `[2]` was a string that is **not valid JSON**;
 *   3. the response was an **error/refusal envelope** the server put in the wire
 *      rather than in the HTTP status.
 *
 * The record could not say which, so diagnosing it cost a second logged-in
 * session. And because none of the three was recognised as a refusal, all three
 * were recorded as `shape-changed` — **permanent**, "the API changed" — even where
 * the platform had refused the request in its own status slot.
 *
 * ## What is asserted here
 *  · The reason is now the specific one `toEntry` recorded, not a fold of them.
 *  · The halt detail carries **shape only** — entry arity, which slot failed, the
 *    numeric status code — and never a value from the response.
 *  · A refusal the envelope names (`wrb.fr[5][0]`) becomes `auth-refused` /
 *    `refused-unknown`: **transient**, so the leg comes back on its ladder, rather
 *    than a permanent `shape-changed`.
 *  · **Nothing is loosened.** An entry with no document and *no* status code is
 *    still `shape-changed`, and the two measured end-of-list signals (an absent
 *    token, an empty items array) are still read as completion — a bare absent
 *    document is never read as "your account is fully enumerated".
 *
 * 🔴 All fixtures are **synthetic**, hand-written from the positions the parser's
 *    own evidence table records. No request goes to gemini.google.com, there is no
 *    logged-in state, and no real conversation, account, token or id appears
 *    below. The http port is injected and throws on a request it was not given.
 */

import { describe, expect, it } from 'vitest';
import { runBackfill, type HttpPort, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import type { BackfillPace, Clock } from '../lib/backfill/pace';
import { haltClassOf } from '../lib/backfill/types';
import { GEMINI_PLAN, parseGeminiListPage } from '../lib/backfill/enumerate';
import {
  GEMINI_AUTH_REFUSAL_CODES,
  GEMINI_RPC_DETAIL,
  GEMINI_RPC_LIST,
  geminiEnvelopeRefusal,
  readListResponse,
  selectRpcPayload,
  parseBatchExecute,
} from '../lib/gemini-rpc';

const ORIGIN = 'https://gemini.google.com';
/** The `c_`-prefixed canonical form — what the list endpoint returns. */
const ID = 'c_1a2b3c4d5e6f708192a3b4c5d6e7f809';
const ID2 = 'c_0f9e8d7c6b5a49382716f5e4d3c2b1a0';
/** A title that must never reach a halt detail. */
const TITLE = 'synthetic-title-that-must-not-leak';

// --- Synthetic envelope fixtures --------------------------------------------

/**
 * The guarded body: `)]}'`, then one length-prefixed frame per chunk. The declared
 * length is the chunk's code-unit count **+2**, the measured framing quirk, so the
 * fixtures exercise the validated path and not only the newline fallback.
 */
function guarded(frames: unknown[]): string {
  let out = ")]}'\n";
  for (const frame of frames) {
    const chunk = JSON.stringify(frame);
    out += `${chunk.length + 2}\n${chunk}\n`;
  }
  return out;
}

/**
 * One `wrb.fr` entry, seven fields, with an optional status slot at index 5.
 *
 * `doc` is the **document slot as the wire carries it**: a JSON *string*. A plain
 * object or array handed in is stringified, because that is what the real entry
 * holds; `null` is left as `null`, because "the slot held nothing" is a different
 * fixture from "the slot held the JSON text `null`".
 */
function wrbEntry(rpcid: unknown, doc: unknown, statusSlot?: unknown): unknown[] {
  const slot = doc === null || doc === undefined
    ? null
    : typeof doc === 'string' ? doc : JSON.stringify(doc);
  return ['wrb.fr', rpcid, slot, null, null, statusSlot ?? null, 'generic'];
}

/** A list page document: `[<unused>, token, items]`. */
function listDoc(ids: string[], token: string | null): unknown[] {
  return [
    null,
    token,
    ids.map((id, index) => [id, index === 0 ? TITLE : `synthetic-title-${index}`, null, null, null, [1_700_000_000 + index, 0]]),
  ];
}

const GOOD_PAGE = guarded([[wrbEntry(GEMINI_RPC_LIST, listDoc([ID], 'synthetic-token'))]]);
const LAST_PAGE = guarded([[wrbEntry(GEMINI_RPC_LIST, listDoc([], null))]]);
/** What an in-band refusal measured in the envelope looks like to this parser. */
const refusalPage = (code: number): string => guarded([[wrbEntry(GEMINI_RPC_LIST, null, [code])]]);
/** A document slot that held a string which is not JSON, with no status code. */
const NON_JSON_PAGE = guarded([[wrbEntry(GEMINI_RPC_LIST, '<not json', null)]]);

// --- The synthetic backend ---------------------------------------------------

interface RecordedCall {
  body: string;
}

/** A backend that answers exactly the list pages it was given, in order, and throws otherwise. */
function geminiBackend(listPages: string[]): { http: HttpPort; calls: RecordedCall[] } {
  const calls: RecordedCall[] = [];
  let cursor = 0;
  const http: HttpPort = async (_url, init) => {
    calls.push({ body: init?.body ?? '' });
    const page = listPages[cursor];
    if (page === undefined) throw new Error('the list was asked for a page it does not have');
    cursor += 1;
    return { status: 200, text: page };
  };
  return { http, calls };
}

function fakeClock(): Clock {
  let time = Date.parse('2026-10-02T00:00:00.000Z');
  return { now: () => time, async sleep(ms: number) { time += ms; } };
}

const NO_WAIT: BackfillPace = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function run(
  store: ReturnType<typeof memoryStore>,
  http: HttpPort,
  scope: string,
): ReturnType<typeof runBackfill> {
  return runBackfill({
    platform: 'gemini',
    origin: ORIGIN,
    scope,
    store,
    http,
    clock: fakeClock(),
    pace: NO_WAIT,
    // 🔴 Every case here is about the LIST segment, so the body segment is switched
    //    off: with it on, the first tick would immediately fetch a body and halt on
    //    whatever this list-only backend answered, which is not the fact under test.
    maxDetails: 0,
    sink: (): SinkOutcome => ({ saved: true, sessionId: ID }),
  });
}

// ---------------------------------------------------------------------------
// 1 · The reason is the specific fact, not a fold of three
// ---------------------------------------------------------------------------
describe('W308-1 · a payload failure names which boundary failed', () => {
  it('names an absent document slot and carries arity and status code, not the old one word', () => {
    const selected = selectRpcPayload(parseBatchExecute(refusalPage(7)), GEMINI_RPC_LIST);
    expect(selected.ok).toBe(false);
    if (selected.ok) return;
    expect(selected.reason).toBe('inner-payload-absent');
    // The shape-only evidence: which slot, the entry's arity, the numeric code.
    expect(selected.detail).toContain('an array of 7 fields');
    expect(selected.detail).toContain('document slot (index 2) is absent');
    expect(selected.detail).toContain('status code 7 in slot 5');
  });

  it('names a document slot that held a non-JSON string', () => {
    const selected = selectRpcPayload(parseBatchExecute(NON_JSON_PAGE), GEMINI_RPC_LIST);
    expect(selected.ok).toBe(false);
    if (selected.ok) return;
    expect(selected.reason).toBe('inner-payload-not-json');
    expect(selected.detail).toContain('a string that is not valid JSON');
    expect(selected.detail).toContain('no status code in slot 5');
  });

  it('names a response that carried no entry for the rpcid at all', () => {
    const body = guarded([[wrbEntry(GEMINI_RPC_DETAIL, [null, null])]]);
    const selected = selectRpcPayload(parseBatchExecute(body), GEMINI_RPC_LIST);
    expect(selected.ok).toBe(false);
    if (selected.ok) return;
    expect(selected.reason).toBe('rpcid-absent');
    expect(selected.detail).toContain('no wrb.fr entry named MaZiqc');
  });

  it('the halt sentence carries the reader’s evidence, and no response value', () => {
    const parsed = parseGeminiListPage(refusalPage(7));
    expect(parsed.ok).toBe(false);
    if (parsed.ok) return;
    expect(parsed.detail).toContain('inner-payload-absent');
    expect(parsed.detail).toContain('status code 7');
    // 🔴 The guard this whole file exists for: a title this page never carried, but
    //    which a leaky diagnostic could have reached through the document.
    expect(parsed.detail).not.toContain(TITLE);
    expect(parsed.detail).not.toContain(ID);
  });
});

// ---------------------------------------------------------------------------
// 2 · A refusal in the envelope is a refusal, not a shape change
// ---------------------------------------------------------------------------
describe('W308-2 · the envelope’s own status slot names a refusal', () => {
  it('reads the one code a source was read calling an authentication failure', () => {
    expect(GEMINI_AUTH_REFUSAL_CODES).toEqual([7]);
    const refusal = geminiEnvelopeRefusal(refusalPage(7));
    expect(refusal?.reason).toBe('auth-refused');
    expect(refusal?.detail).toContain('status code 7');
    expect(refusal?.detail).toContain('permission-denied');
  });

  it('records any other non-zero code as unreadable rather than guessing a login problem', () => {
    const refusal = geminiEnvelopeRefusal(refusalPage(8));
    expect(refusal?.reason).toBe('refused-unknown');
    expect(refusal?.detail).toContain('status code 8');
    expect(refusal?.detail).toContain('does not recognise this code');
    // It must not tell a user to log in about a code nothing was read for.
    expect(refusal?.detail.toLowerCase()).not.toContain('log in');
  });

  it('does not read a success, an absent slot or a non-number as a refusal', () => {
    // code 0 = no verdict; absent slot = no verdict; a string "7" is not a code.
    expect(geminiEnvelopeRefusal(guarded([[wrbEntry(GEMINI_RPC_LIST, null, [0])]]))).toBeNull();
    expect(geminiEnvelopeRefusal(guarded([[wrbEntry(GEMINI_RPC_LIST, null, null)]]))).toBeNull();
    expect(geminiEnvelopeRefusal(guarded([[wrbEntry(GEMINI_RPC_LIST, null, ['7'])]]))).toBeNull();
    expect(geminiEnvelopeRefusal(GOOD_PAGE)).toBeNull();
    // Not this envelope at all — including the bundle the detail segment is handed.
    expect(geminiEnvelopeRefusal('{"rpcid":"hNvQHb","conversationId":"c_x","pages":["x"]}')).toBeNull();
    expect(geminiEnvelopeRefusal('not a batchexecute body')).toBeNull();
  });

  it('does not call a working response a refusal just because a code rode along', () => {
    // A usable document with a code this build cannot read is left to the shape
    // checks, which name the code — classifying it a refusal would put a working
    // endpoint on the backoff ladder.
    const usable = guarded([[wrbEntry(GEMINI_RPC_DETAIL, [null, null], [9])]]);
    expect(geminiEnvelopeRefusal(usable)).toBeNull();
    // ...but the one code a source *was* read calling a credential failure still
    // wins even beside a document, because that is what the source claims about it.
    const withDoc = guarded([[wrbEntry(GEMINI_RPC_DETAIL, [null, null], [7])]]);
    expect(geminiEnvelopeRefusal(withDoc)?.reason).toBe('auth-refused');
  });
});

// ---------------------------------------------------------------------------
// 3 · The engine halts transiently, and loosens nothing
// ---------------------------------------------------------------------------
describe('W308-3 · what the leg does with each kind of page', () => {
  it('an in-band refusal is a transient halt, so the leg comes back by itself', async () => {
    const store = memoryStore();
    const { http } = geminiBackend([GOOD_PAGE, refusalPage(7)]);
    const first = await run(store, http, 'acct-inband');
    // The first page enumerates ID; the second is refused in the envelope.
    expect(first.state.enumCursor.complete).toBe(false);
    const second = await run(store, http, 'acct-inband');
    expect(second.halted?.reason).toBe('auth-refused');
    expect(haltClassOf('auth-refused')).toBe('transient');
    expect(second.halted?.detail).toContain('status code 7');
    // 🔴 Not the old answer, and not a completion either.
    expect(second.halted?.reason).not.toBe('shape-changed');
    expect(second.state.enumCursor.complete).toBe(false);
  });

  it('an unreachable document with no status code is still a permanent shape halt', async () => {
    const store = memoryStore();
    const { http } = geminiBackend([GOOD_PAGE, guarded([[wrbEntry(GEMINI_RPC_LIST, null, null)]])]);
    await run(store, http, 'acct-nostatus');
    const second = await run(store, http, 'acct-nostatus');
    // 🔴 The fix must not turn "we could not read this page" into "you are done".
    expect(second.halted?.reason).toBe('shape-changed');
    expect(haltClassOf('shape-changed')).toBe('permanent');
    expect(second.halted?.detail).toContain('inner-payload-absent');
    expect(second.halted?.detail).toContain('no status code in slot 5');
    expect(second.state.enumCursor.complete).toBe(false);
  });

  it('a non-JSON document is named as such, not folded into the same sentence', async () => {
    const store = memoryStore();
    const { http } = geminiBackend([GOOD_PAGE, NON_JSON_PAGE]);
    await run(store, http, 'acct-notjson');
    const second = await run(store, http, 'acct-notjson');
    expect(second.halted?.reason).toBe('shape-changed');
    expect(second.halted?.detail).toContain('inner-payload-not-json');
    // The distinction the old one word could not carry.
    expect(second.halted?.detail).not.toContain('inner-payload-absent');
  });

  it('the two end-of-list signals are still read as completion, not as a shape change', async () => {
    const store = memoryStore();
    const { http, calls } = geminiBackend([GOOD_PAGE, LAST_PAGE]);
    await run(store, http, 'acct-end');
    const second = await run(store, http, 'acct-end');
    expect(second.halted).toBeNull();
    expect(second.state.enumCursor.complete).toBe(true);
    // The leg stopped because the server said so — two pages, then no more — and
    // not because it stopped asking.
    expect(calls).toHaveLength(2);
  });
});

// ---------------------------------------------------------------------------
// 4 · The reader the plan is wired to
// ---------------------------------------------------------------------------
describe('W308-4 · the plan declares the refusal', () => {
  it('the Gemini plan asks the envelope reader before any shape judgement', () => {
    expect(GEMINI_PLAN.refusalOf).toBe(geminiEnvelopeRefusal);
    expect(GEMINI_PLAN.parseListPage).toBe(parseGeminiListPage);
  });

  it('a list page that reads is unaffected: the refusal hook is null for it', () => {
    const read = readListResponse(GOOD_PAGE);
    expect(read.ok).toBe(true);
    if (!read.ok) return;
    expect(read.ids).toEqual([ID]);
    expect(read.nextPageToken).toBe('synthetic-token');
  });
});
