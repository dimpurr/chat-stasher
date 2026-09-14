/**
 * W26 · The Gemini `batchexecute` envelope parser: the frame walk, the two-level JSON, the
 * request body, and the positional readers for the list and detail payloads.
 *
 * ## What this file exists to stop changing back
 *  1. **A declared frame length is a claim, not an instruction.** The parser prefers the
 *     declared slice only when it lands on a real frame boundary *and* parses. A length
 *     counted in the wrong unit, or one that overshoots into the next frame, degrades to the
 *     newline-delimited chunk and is counted — it never yields a truncated payload that
 *     happens to parse.
 *  2. **Unreadable is not empty.** A frame whose JSON never decoded is counted; an entry whose
 *     inner document failed to parse carries a named `error`; a list or detail path that is
 *     absent is a named shape error. None of those may come back as a zero-length result.
 *  3. **Unknown entry kinds survive.** A `wrb.fr` frame is not the only kind in these bodies;
 *     the others are kept verbatim and counted, because a kind that is dropped silently cannot
 *     be told apart from a kind that arrived carrying nothing.
 *  4. **A missing guard is recorded, not assumed in either direction.** Bodies with and without
 *     the `)]}'` line both parse; the result says which one this was.
 *
 * 🔴 All fixtures below are **hand-written**, built by the two helpers at the top of this file.
 *    They are shaped like the envelope the sources describe; they contain no real conversation,
 *    no account, and no text copied from anywhere. Nothing here opens a socket or a browser:
 *    every function under test is pure.
 */

import { describe, expect, it } from 'vitest';
import {
  buildBatchExecuteBody,
  extractDetailPage,
  extractListPage,
  FRAME_LENGTH_UNIT,
  GEMINI_RPC_DETAIL,
  GEMINI_RPC_LIST,
  parseBatchExecute,
} from '../lib/gemini-rpc';

// --- Hand-written envelope fixtures -----------------------------------------
//
// The shape under test is the one the sources describe:
//
//   )]}'
//   <declared length, in UTF-16 code units>
//   [["wrb.fr","<rpcid>","<inner JSON, encoded as a string>",null,null,null,"generic"]]
//   <declared length>
//   [["di",87],["af.httprm",87,"-1000000000000000000",4]]

/** One frame: a length line, then the chunk, then the separating newline. */
function frame(value: unknown, declaredLength?: number): string {
  const chunk = JSON.stringify(value);
  return `${declaredLength ?? chunk.length}\n${chunk}\n`;
}

/** A whole guarded body. `guard: false` drops the anti-JSON line. */
function envelope(frames: string[], guard = true): string {
  return (guard ? ")]}'\n\n" : '') + frames.join('');
}

/** A `wrb.fr` frame carrying `payload` as its inner document. */
function rpcFrame(rpcid: string, payload: unknown, declaredLength?: number): string {
  return frame([['wrb.fr', rpcid, JSON.stringify(payload), null, null, null, 'generic']], declaredLength);
}

/** The trailing frames every real response ends with. */
const TRAILER = () => [frame([['di', 87]]), frame([['e', 4, null, null, 100]])];

/** Multi-byte content that is deliberately NOT CJK: the repo bans CJK outside the locale file. */
const ACCENTED = 'café — Ω — naïve';
/** An astral character: two UTF-16 code units, four UTF-8 bytes. */
const ASTRAL = '🗂';

/**
 * `list[index]`, with the index asserted to exist.
 *
 * The test suite compiles under `noUncheckedIndexedAccess`, so a bare `list[0]`
 * is `T | undefined` and every assertion on it would need a guard that says
 * nothing. A missing element here is a broken test, not a finding, so it is
 * asserted once, here, and the rest of the file stays about the parser.
 */
function at<T>(list: T[], index: number): T {
  expect(list[index], `no element at index ${index} (length ${list.length})`).toBeDefined();
  return list[index]!;
}

// --- The frame walk ---------------------------------------------------------

describe('parseBatchExecute · the envelope', () => {
  it('reads every entry of a multi-chunk response, in order', () => {
    const body = envelope([
      rpcFrame(GEMINI_RPC_LIST, [null, 'TOKEN_NEXT', [['c_0001', 'first']]]),
      rpcFrame(GEMINI_RPC_DETAIL, [[[['c_0001', 'r_0001']]]]),
      ...TRAILER(),
    ]);

    const result = parseBatchExecute(body);
    expect(result.ok).toBe(true);
    if (!result.ok) return;

    expect(result.guardPresent).toBe(true);
    expect(result.entries.map((e) => e.rpcid)).toEqual([GEMINI_RPC_LIST, GEMINI_RPC_DETAIL, null, null]);
    // The two trailing diagnostic frames are kept and counted, not dropped.
    expect(result.unknownEntryCount).toBe(2);
    expect(result.undecodedFrameCount).toBe(0);
    expect(result.nonEntryArrayFrameCount).toBe(0);
    expect(result.lengthPrefixMismatchCount).toBe(0);
  });

  it('keeps an unrecognised entry kind verbatim instead of dropping it', () => {
    const stranger = ['future.kind', 'opaque', { nested: true }];
    const result = parseBatchExecute(envelope([frame([stranger]), ...TRAILER()]));

    expect(result.ok).toBe(true);
    if (!result.ok) return;

    const kept = at(result.entries, 0);
    expect(kept.recognized).toBe(false);
    expect(kept.label).toBe('future.kind');
    expect(kept.rpcid).toBeNull();
    expect(kept.payload).toBeNull();
    expect(kept.error).toBeUndefined();
    // Nothing is lost: the original entry rides along untouched.
    expect(kept.raw).toEqual(stranger);
    expect(result.unknownEntryCount).toBe(result.entries.length);
  });

  it('counts an entry kind it does not know without calling it an error', () => {
    const result = parseBatchExecute(envelope([rpcFrame(GEMINI_RPC_LIST, []), ...TRAILER()]));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    // `di` and `e` are the shapes this parser has seen and does not interpret.
    expect(result.entries.filter((e) => e.label === 'di')).toHaveLength(1);
    expect(result.entries.filter((e) => e.label === 'e')).toHaveLength(1);
    expect(result.entries.some((e) => e.error !== undefined)).toBe(false);
  });
});

describe('parseBatchExecute · the anti-JSON guard', () => {
  it('parses a body whose guard is missing and says so', () => {
    const body = envelope([rpcFrame(GEMINI_RPC_LIST, [null, null, [['c_0001', 'title']]])], false);
    expect(body.startsWith(")]}'")).toBe(false);

    const result = parseBatchExecute(body);
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    // "the guard was not there" and "we did not look" are different facts, and
    // only one of them is true here.
    expect(result.guardPresent).toBe(false);
    expect(at(result.entries, 0).rpcid).toBe(GEMINI_RPC_LIST);
  });

  it('does not treat a guarded body and a bare one as the same reading', () => {
    const guarded = parseBatchExecute(envelope([rpcFrame(GEMINI_RPC_LIST, [])], true));
    const bare = parseBatchExecute(envelope([rpcFrame(GEMINI_RPC_LIST, [])], false));
    expect(guarded.ok && bare.ok).toBe(true);
    if (!guarded.ok || !bare.ok) return;
    expect(guarded.guardPresent).toBe(true);
    expect(bare.guardPresent).toBe(false);
  });

  it('parses a body that is plain newline-delimited JSON, with no length lines at all', () => {
    // No `frame()` wrapper: these lines carry no length prefix at all, so the
    // newline is the only boundary the parser has to go on.
    const result = parseBatchExecute(
      envelope([`${JSON.stringify([['wrb.fr', GEMINI_RPC_LIST, '[]', null]])}\n`]),
    );
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.entries, 0).rpcid).toBe(GEMINI_RPC_LIST);
    expect(at(result.entries, 0).payload).toEqual([]);
  });
});

describe('parseBatchExecute · the length prefix is checked, not obeyed', () => {
  it('uses the declared length when it is right, and reports no mismatch', () => {
    const chunk = JSON.stringify([['wrb.fr', GEMINI_RPC_LIST, '[]', null, null, null, 'generic']]);
    const result = parseBatchExecute(envelope([`${chunk.length}\n${chunk}\n`, ...TRAILER()]));

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.lengthPrefixMismatchCount).toBe(0);
    expect(at(result.entries, 0).payload).toEqual([]);
  });

  it('counts a length that swallows the frame separator as a mismatch', () => {
    // A declared length one unit too long — the reading in which the number
    // counts the chunk *and* its terminating newline. The slice that produces is
    // still valid JSON (trailing whitespace is legal), so nothing about the value
    // reveals the disagreement; only the mismatch count does. Without a boundary
    // check this framing is accepted silently and the count stays at zero.
    const chunk = JSON.stringify([['wrb.fr', GEMINI_RPC_LIST, '[]', null, null, null, 'generic']]);
    const result = parseBatchExecute(envelope([`${chunk.length + 1}\n${chunk}\n`, ...TRAILER()]));

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.entries, 0).payload).toEqual([]);
    expect(result.lengthPrefixMismatchCount).toBe(1);
  });

  it('falls back to the newline chunk when the declared length is counted in bytes', () => {
    // Counted in bytes: longer than the code-unit length exactly when the frame
    // carries multi-byte text, so the declared slice overshoots into the next frame.
    const text = `${ACCENTED} ${ASTRAL}`;
    const chunk = JSON.stringify([['wrb.fr', GEMINI_RPC_DETAIL, JSON.stringify([text]), null]]);
    expect(Buffer.byteLength(chunk, 'utf8')).toBeGreaterThan(chunk.length);

    const result = parseBatchExecute(
      envelope([`${Buffer.byteLength(chunk, 'utf8')}\n${chunk}\n`, ...TRAILER()]),
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.entries, 0).payload).toEqual([text]);
    expect(at(result.entries, 0).error).toBeUndefined();
    expect(result.lengthPrefixMismatchCount).toBe(1);
  });

  it('still reads the frame after a length that overshoots its chunk', () => {
    // A declared length two units too long: the slice swallows the newline and
    // the first digit of the next header. Naive slicing would then lose that digit.
    const chunk = JSON.stringify([['wrb.fr', GEMINI_RPC_LIST, '[]', null, null, null, 'generic']]);
    const result = parseBatchExecute(envelope([`${chunk.length + 2}\n${chunk}\n`, ...TRAILER()]));

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.lengthPrefixMismatchCount).toBe(1);
    expect(at(result.entries, 0).rpcid).toBe(GEMINI_RPC_LIST);
    // The next frame still arrives intact — proof the walk did not eat its header.
    expect(at(result.entries, 1).label).toBe('di');
    expect(result.unknownEntryCount).toBe(2);
  });

  it('hands back the chunk unchanged when a multi-byte character sits at its end', () => {
    const text = `tail is ${ASTRAL}`;
    const chunk = JSON.stringify([['wrb.fr', GEMINI_RPC_DETAIL, JSON.stringify([text]), null]]);
    // The premise has to be real for the assertion to mean anything: this chunk
    // must be shorter in code units than it is in bytes.
    expect(chunk.length).toBeLessThan(Buffer.byteLength(chunk, 'utf8'));

    const result = parseBatchExecute(envelope([`${chunk.length}\n${chunk}\n`, ...TRAILER()]));

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    // The astral character survives whole: a slice counted in bytes would have
    // cut its surrogate pair in half and left a replacement character behind.
    const payload = at(result.entries, 0).payload;
    expect(payload).toEqual([text]);
    expect(JSON.stringify(payload)).toContain(ASTRAL);
    expect(FRAME_LENGTH_UNIT).toBe('utf16-code-unit');
  });
});

describe('parseBatchExecute · malformed input is named, never thrown', () => {
  it('reports a truncated final chunk as undecoded and keeps the readable frames', () => {
    const good = rpcFrame(GEMINI_RPC_LIST, [null, null, [['c_0001', 'title']]]);
    const truncatedBody = envelope([good]) + `${JSON.stringify([['wrb.fr', GEMINI_RPC_DETAIL, '[[[', null]]).length}\n[["wrb.fr","${GEMINI_RPC_DETAIL}","[[[`;

    const result = parseBatchExecute(truncatedBody);
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.undecodedFrameCount).toBe(1);
    expect(result.entries).toHaveLength(1);
    expect(at(result.entries, 0).rpcid).toBe(GEMINI_RPC_LIST);
  });

  it('reports a body whose every frame is undecodable as a failure, not as zero entries', () => {
    const result = parseBatchExecute(envelope([`40\n[["wrb.fr","MaZiqc","[[[`]));
    expect(result).toEqual({ ok: false, reason: 'frames-undecodable' });
  });

  it('reports a body with no recognisable frame as not a batchexecute body', () => {
    expect(parseBatchExecute('this is not an envelope')).toEqual({
      ok: false,
      reason: 'not-a-batchexecute-body',
    });
    expect(parseBatchExecute('{"error":"unauthenticated"}')).toEqual({
      ok: false,
      reason: 'not-a-batchexecute-body',
    });
  });

  it('separates an empty response from an unreadable one', () => {
    expect(parseBatchExecute('')).toEqual({ ok: false, reason: 'empty-response' });
    expect(parseBatchExecute('   \n\t ')).toEqual({ ok: false, reason: 'empty-response' });
    expect(parseBatchExecute(undefined as unknown as string)).toEqual({
      ok: false,
      reason: 'not-a-string',
    });
  });

  it('never throws, whatever it is handed', () => {
    const hostile = [
      ")]}'",
      ")]}'\n",
      ")]}'\n\n999999999\n",
      ")]}'\n\n0\n[]\n",
      ")]}'\n\n-1\n[]\n",
      ")]}'\n\nabc\n[]\n",
      ')]}\'\n\n2\n[}\n',
      ')]}\'\n\n1\n[\n1\n[\n',
    ];
    for (const text of hostile) {
      expect(() => parseBatchExecute(text)).not.toThrow();
    }
  });

  it('counts a frame that decodes to something other than an entry array', () => {
    const result = parseBatchExecute(envelope([frame(5), rpcFrame(GEMINI_RPC_LIST, []), ...TRAILER()]));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.nonEntryArrayFrameCount).toBe(1);
    expect(at(result.entries, 0).rpcid).toBe(GEMINI_RPC_LIST);
  });
});

describe('parseBatchExecute · entries that carry no usable payload', () => {
  it('names an absent inner document instead of reporting an empty one', () => {
    const body = envelope([
      frame([['wrb.fr', GEMINI_RPC_DETAIL, null, null, null, [13], 'generic']]),
      ...TRAILER(),
    ]);
    const result = parseBatchExecute(body);
    expect(result.ok).toBe(true);
    if (!result.ok) return;

    const entry = at(result.entries, 0);
    expect(entry.rpcid).toBe(GEMINI_RPC_DETAIL);
    expect(entry.payload).toBeNull();
    expect(entry.error).toBe('inner-payload-absent');
    expect(entry.recognized).toBe(true);
  });

  it('names inner JSON that does not parse rather than falling back to the raw string', () => {
    const body = envelope([frame([['wrb.fr', GEMINI_RPC_DETAIL, '[["broken"', null]]), ...TRAILER()]);
    const result = parseBatchExecute(body);
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.entries, 0).payload).toBeNull();
    expect(at(result.entries, 0).error).toBe('inner-payload-not-json');
    expect((at(result.entries, 0).raw as unknown[])[2]).toBe('[["broken"');
  });

  it('names an RPC entry with no id rather than attributing its payload to nothing', () => {
    const result = parseBatchExecute(envelope([frame([['wrb.fr', 5, '[]', null]]), ...TRAILER()]));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.entries, 0).rpcid).toBeNull();
    expect(at(result.entries, 0).error).toBe('rpcid-absent');
    expect(at(result.entries, 0).payload).toBeNull();
  });

  it('tells a decoded JSON null apart from a payload that was not there', () => {
    const decodedNull = parseBatchExecute(envelope([rpcFrame(GEMINI_RPC_LIST, null), ...TRAILER()]));
    const absent = parseBatchExecute(
      envelope([frame([['wrb.fr', GEMINI_RPC_LIST, null, null]]), ...TRAILER()]),
    );
    expect(decodedNull.ok && absent.ok).toBe(true);
    if (!decodedNull.ok || !absent.ok) return;

    // Both report `payload: null`. Only one of them is an error, and the caller
    // can tell which — which is the entire reason `error` exists.
    expect(at(decodedNull.entries, 0).payload).toBeNull();
    expect(at(decodedNull.entries, 0).error).toBeUndefined();
    expect(at(absent.entries, 0).payload).toBeNull();
    expect(at(absent.entries, 0).error).toBe('inner-payload-absent');
  });
});

// --- The request body -------------------------------------------------------

describe('buildBatchExecuteBody', () => {
  const AT = 'AO7uq-xO9W1lN2s_';

  it('builds the two-field form body with the batch nested three levels deep', () => {
    const body = buildBatchExecuteBody(GEMINI_RPC_DETAIL, ['c_0001', 100, null, 1, [0], [4], null, 1], AT);

    const params = new URLSearchParams(body);
    expect([...params.keys()].sort()).toEqual(['at', 'f.req']);
    expect(params.get('at')).toBe(AT);

    const outer = JSON.parse(params.get('f.req') as string);
    expect(outer).toHaveLength(1);
    expect(outer[0][0][0]).toBe(GEMINI_RPC_DETAIL);
    // The middle level is a *string*, not an object: that nesting is the point.
    expect(typeof outer[0][0][1]).toBe('string');
    expect(JSON.parse(outer[0][0][1])).toEqual(['c_0001', 100, null, 1, [0], [4], null, 1]);
    expect(outer[0][0][2]).toBeNull();
    expect(outer[0][0][3]).toBe('generic');
  });

  it('encodes the token so a separator in it cannot break the body open', () => {
    const at = 'a&b=c+d e';
    const body = buildBatchExecuteBody(GEMINI_RPC_LIST, [], at);
    expect(body).not.toContain('a&b');
    expect(new URLSearchParams(body).get('at')).toBe(at);
  });

  it('keeps a quote in the args inside the inner string', () => {
    const args = ['c_0001', 'say "hi"', null];
    const body = buildBatchExecuteBody(GEMINI_RPC_DETAIL, args, AT);
    const outer = JSON.parse(new URLSearchParams(body).get('f.req') as string);
    expect(JSON.parse(outer[0][0][1])).toEqual(args);
  });
});

// --- The list reader --------------------------------------------------------

describe('extractListPage', () => {
  /** `[null, token, [item, …]]` — the triple the sources describe. */
  function listPayload(items: unknown[], token: string | null): unknown {
    return [null, token, items];
  }

  function item(id: string, title: string | null, seconds?: number): unknown[] {
    const row: unknown[] = [id, title, null, null, null];
    if (seconds !== undefined) row.push([seconds, 0]);
    return row;
  }

  it('reads the id, the title and the creation time of every entry', () => {
    const result = extractListPage(
      listPayload([item('c_0001', 'first', 1700000000), item('c_0002', 'second')], 'TOKEN_NEXT'),
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.nextPageToken).toBe('TOKEN_NEXT');
    expect(result.pageIsEmpty).toBe(false);
    expect(result.conversations).toEqual([
      { id: 'c_0001', title: 'first', createdAtSeconds: 1700000000 },
      { id: 'c_0002', title: 'second', createdAtSeconds: null },
    ]);
  });

  it('reads an empty page with a token as empty, and still hands back the token', () => {
    const result = extractListPage(listPayload([], 'TOKEN_NEXT'));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    // These two are different facts and the result keeps them apart: the page
    // listed nothing, and the server did offer somewhere to go next.
    expect(result.pageIsEmpty).toBe(true);
    expect(result.conversations).toEqual([]);
    expect(result.nextPageToken).toBe('TOKEN_NEXT');
  });

  it('reads an empty page without a token as the end of the list', () => {
    const result = extractListPage(listPayload([], null));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.pageIsEmpty).toBe(true);
    expect(result.nextPageToken).toBeNull();
  });

  it('reports a missing items path as a shape error, never as an empty list', () => {
    expect(extractListPage([null, null])).toEqual({ ok: false, reason: 'list-items-path-missing' });
    expect(extractListPage([null, 'TOKEN', 'not-an-array'])).toEqual({
      ok: false,
      reason: 'list-items-path-missing',
    });
    expect(extractListPage(null)).toEqual({ ok: false, reason: 'payload-not-an-array' });
    expect(extractListPage('nope')).toEqual({ ok: false, reason: 'payload-not-an-array' });
  });

  it('keeps an empty title empty rather than turning it into an absent one', () => {
    const result = extractListPage(listPayload([item('c_0001', '')], null));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.conversations, 0).title).toBe('');
    expect(at(result.conversations, 0).title).not.toBeNull();
  });

  it('keeps an item with no id rather than dropping it or inventing one', () => {
    const result = extractListPage(
      listPayload([['', 'empty id'], item('c_0002', 'second'), {}, item('c_0003', null)], null),
    );
    expect(result.ok).toBe(true);
    if (!result.ok) return;

    // The page carried four items and the reader reports four. An item it cannot
    // address is not the same thing as an item that was not there, and a reader
    // that dropped one would make an unaddressable page look like an empty one.
    expect(result.conversations.map((c) => c.id)).toEqual(['', 'c_0002', null, 'c_0003']);
    expect(result.pageIsEmpty).toBe(false);
    expect(at(result.conversations, 3).title).toBeNull();
  });
});

// --- The detail reader ------------------------------------------------------

describe('extractDetailPage', () => {
  function turn(conversationId: string, responseId: string, seconds?: number, nanos?: number): unknown[] {
    const row: unknown[] = [[conversationId, responseId], null, null, null];
    row.push(seconds === undefined ? null : [seconds, nanos ?? 0]);
    return row;
  }

  function detailPayload(turns: unknown[], token: string | null): unknown {
    return [turns, token, null];
  }

  it('reads the ids a page has to be deduped by', () => {
    const result = extractDetailPage(
      detailPayload([turn('c_0001', 'r_0002', 1700000001, 5), turn('c_0001', 'r_0001', 1700000000)], null),
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;
    // Newest-first, exactly as the response gave them: reversal is the caller's job.
    expect(result.turns.map((t) => t.responseId)).toEqual(['r_0002', 'r_0001']);
    expect(at(result.turns, 0)).toEqual({
      conversationId: 'c_0001',
      responseId: 'r_0002',
      timestampSeconds: 1700000001,
      timestampNanos: 5,
    });
  });

  it('hands back the token into the older turns', () => {
    const result = extractDetailPage(detailPayload([turn('c_0001', 'r_0001')], 'OLDER_TOKEN'));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.nextPageToken).toBe('OLDER_TOKEN');
    expect(result.pageIsEmpty).toBe(false);
  });

  it('reports an empty page with and without a token', () => {
    const withToken = extractDetailPage(detailPayload([], 'OLDER_TOKEN'));
    const atEnd = extractDetailPage(detailPayload([], null));
    expect(withToken.ok && atEnd.ok).toBe(true);
    if (!withToken.ok || !atEnd.ok) return;
    expect(withToken.pageIsEmpty).toBe(true);
    expect(withToken.nextPageToken).toBe('OLDER_TOKEN');
    expect(atEnd.pageIsEmpty).toBe(true);
    expect(atEnd.nextPageToken).toBeNull();
  });

  it('reports a missing turns path as a shape error, never as an empty page', () => {
    expect(extractDetailPage([null, null, null])).toEqual({
      ok: false,
      reason: 'detail-turns-path-missing',
    });
    expect(extractDetailPage({ payload: [] })).toEqual({ ok: false, reason: 'payload-not-an-array' });
  });

  it('reports absent ids and an absent timestamp as null, not as empty strings or zero', () => {
    const result = extractDetailPage(detailPayload([[null, null, null, null, null]], null));
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(at(result.turns, 0)).toEqual({
      conversationId: null,
      responseId: null,
      timestampSeconds: null,
      timestampNanos: null,
    });
  });
});

// --- The two readers feeding off the parser ---------------------------------

describe('the parser and the readers agree on a whole response', () => {
  it('parses a body and reads the list page out of the entry it carried', () => {
    const payload = [null, 'TOKEN_NEXT', [['c_0001', 'first', null, null, null, [1700000000, 0]]]];
    const body = envelope([rpcFrame(GEMINI_RPC_LIST, payload), ...TRAILER()]);

    const parsed = parseBatchExecute(body);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;

    const entry = parsed.entries.find((e) => e.rpcid === GEMINI_RPC_LIST);
    expect(entry).toBeDefined();
    if (entry === undefined) return;
    expect(entry.error).toBeUndefined();

    // The reader is handed the entry the parser produced, not a re-parsed
    // fixture: this is the one place the two halves are checked against each other.
    const page = extractListPage(entry.payload);
    expect(page.ok).toBe(true);
    if (!page.ok) return;
    expect(page.conversations).toEqual([
      { id: 'c_0001', title: 'first', createdAtSeconds: 1700000000 },
    ]);
    expect(page.nextPageToken).toBe('TOKEN_NEXT');
  });
});
