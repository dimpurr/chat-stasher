/**
 * A standalone parser for Gemini's `batchexecute` RPC envelope.
 *
 * Gemini's web app loads a conversation with a POST to
 * `/_/BardChatUi/data/batchexecute?rpcids=…` and answers in a format that is not
 * JSON: an anti-JSON guard line, then length-prefixed chunks, each chunk a JSON
 * array whose entries carry a *further* JSON document encoded as a string.
 *
 * ## Why this is its own module
 * The envelope is the part of Gemini capture that is pure: given the response
 * text it is a total function. Nothing here fetches, reads the DOM, touches
 * storage or knows about the backfill engine, so the parsing rules can be
 * stated, tested and argued about without a browser.
 *
 * 🔴 W29 wired it into the capture path (the backfill plan in
 *    lib/backfill/enumerate.ts and the live leg in
 *    entrypoints/dw-bridge.content.ts) and added the readers below — still pure:
 *    "one RPC document out of a parsed response", "one page of a detail
 *    conversation", and the **bundle** that holds every raw page of one
 *    conversation in order. Nothing here decides *whether* to fetch; that
 *    decision is the caller's.
 *
 * ## The two facts that shaped the design
 *
 * 1. **A declared length is not trusted; it is checked.** The sources disagree
 *    in practice about what the number counts, so this parser tries the declared
 *    slice first and falls back to the newline-delimited chunk, recording that
 *    it had to. See `walkFrames` for the measurement behind that choice, and
 *    `FRAME_LENGTH_UNIT` for the unit.
 * 2. **Nothing is reported as empty when it is merely unreadable.** A frame
 *    whose JSON never decoded is *counted*; an entry whose inner document failed
 *    to parse carries a named `error`, never a `null` payload standing in for
 *    one; a list or detail path that is missing is a named shape error, never a
 *    zero-length result. A decoded `null` and an absent value stay distinguishable.
 *
 * 🔴 Every positional index below is a named constant with a comment stating how
 *    strong its evidence is. "One source" means a single project's code asserted
 *    it; "sources agree" means two or more independent ones do. Nothing here was
 *    verified against a live logged-in session by this change.
 */

/** The list RPC (sidebar history). Evidence: sources agree. */
export const GEMINI_RPC_LIST = 'MaZiqc';
/** The detail RPC — the one the page itself uses to load a past conversation. Evidence: sources agree. */
export const GEMINI_RPC_DETAIL = 'hNvQHb';

/**
 * Which unit the frame length prefix counts.
 *
 * **UTF-16 code units — i.e. JavaScript `String.length` — not bytes.**
 *
 * Measured against the one captured response body in scope whose frames carry
 * non-ASCII text. Reading the declared number as a byte count is not merely
 * inaccurate, it is impossible: two of its frames declare 5190 and 21397 where
 * the byte lengths are 5524 and 26870, i.e. the declared number would be
 * *smaller than the chunk it describes*, and a length prefix cannot be negative.
 * The code-unit reading is the only one that stays positive and consistent.
 *
 * Because a JS string is indexed in code units, slicing a code-unit count can
 * never split a surrogate pair, which is why the multi-byte case needs no
 * special handling here — and why a byte-counted prefix, should one ever turn
 * up, degrades to the newline fallback instead of corrupting text.
 *
 * In that same capture every declared number is exactly two units larger than
 * the chunk that follows it, in code-unit space. Whether that is part of the
 * framing or an artifact of how that particular capture was produced is not
 * settled by any source in scope (see the open questions in the task report),
 * which is precisely why the declared length is validated rather than obeyed.
 */
export const FRAME_LENGTH_UNIT = 'utf16-code-unit' as const;

/** The anti-JSON guard the response opens with. */
const GUARD = ")]}'";

/** Named reasons a whole response could not be read as a `batchexecute` body. */
export type BatchExecuteFailureReason =
  /** The input was not a string at all (a caller error, reported rather than thrown). */
  | 'not-a-string'
  /** The input was empty or only whitespace — nothing was there to read. */
  | 'empty-response'
  /** No frame boundary was recognised: this does not look like a `batchexecute` body. */
  | 'not-a-batchexecute-body'
  /** Frame boundaries were found but not one of them decoded to JSON. */
  | 'frames-undecodable';

/** Named reasons an entry inside a decoded frame yielded no usable payload. */
export type BatchExecuteEntryError =
  /** The entry carried no RPC id, so its payload could not be attributed. */
  | 'rpcid-absent'
  /** `entry[2]` was null or missing — an RPC that returned no document. */
  | 'inner-payload-absent'
  /** `entry[2]` was a string but not valid JSON. */
  | 'inner-payload-not-json';

/**
 * One entry of a decoded frame, kept in the flat shape the rest of the capture
 * path can consume: `rpcid` + `payload`, with `error` present exactly when the
 * payload is unusable.
 */
export interface BatchExecuteEntry {
  /** `entry[0]` when it is a string, else null. Kept for unrecognised kinds. */
  label: string | null;
  /** `entry[1]` when it is a string, else null. */
  rpcid: string | null;
  /**
   * The decoded inner document, or null when there is none.
   *
   * A null payload with no `error` is a *decoded* JSON `null`; a null payload
   * with an `error` is a value that could not be read. Those are different
   * facts and this field never merges them.
   */
  payload: unknown | null;
  /** Why the payload is unusable. Absent when the entry decoded. */
  error?: BatchExecuteEntryError;
  /** True only for entries of a kind this parser understands. */
  recognized: boolean;
  /** The entry exactly as it appeared, so a caller can inspect what was rejected. */
  raw: unknown;
}

export type BatchExecuteParseResult =
  | {
      ok: true;
      /** Whether the `)]}'` guard was actually present. A missing guard is recorded, not assumed. */
      guardPresent: boolean;
      entries: BatchExecuteEntry[];
      /** Frames whose JSON never decoded. Counted, never dropped silently. */
      undecodedFrameCount: number;
      /** Frames that decoded but were not an array of entries. */
      nonEntryArrayFrameCount: number;
      /** Frames where the declared length prefix did not match the chunk used. */
      lengthPrefixMismatchCount: number;
      /** Entries of a kind other than `wrb.fr`. Kept in `entries` and counted here. */
      unknownEntryCount: number;
    }
  | { ok: false; reason: BatchExecuteFailureReason };

// --- The frame walk ---------------------------------------------------------

interface Frame {
  /** The decoded JSON of the frame. */
  value: unknown;
  /** The declared length prefix, or null when the chunk had no length line. */
  declaredLength: number | null;
  /** The chunk's length in UTF-16 code units, for comparing against `declaredLength`. */
  actualLength: number;
}

interface FrameWalk {
  frames: Frame[];
  /** Chunks that were recognised as chunks but whose JSON never decoded. */
  undecodedFrameCount: number;
  /** Whether any position looked like this envelope (a length line, or a JSON array line). */
  sawEnvelopeShape: boolean;
}

/**
 * Splits the guarded body into chunks and decodes each one.
 *
 * The walk is deliberately evidence-driven at every step:
 *  - a line of digits is treated as a length prefix; anything else is treated as
 *    a length-less JSON line, because responses in this family also arrive as
 *    plain newline-delimited JSON;
 *  - the declared slice is used **only if it ends on a frame boundary and
 *    parses as JSON**. Both halves matter: the boundary test rejects a length
 *    that overshoots into the next frame, and the parse test rejects one that
 *    undershoots or that is counted in the wrong unit. When either fails, the
 *    newline-delimited chunk is used instead. That is what makes the parser
 *    correct under either reading of the length unit, and under the unexplained
 *    offset: a wrong interpretation of the number degrades to the fallback
 *    instead of producing a truncated payload that still parses.
 *  - a chunk that decodes to something which is not an array is still a decoded
 *    frame here; whether it holds entries is the caller's question.
 *
 * Splitting on newlines is safe because these chunks are `JSON.stringify`
 * output: a newline inside a string is escaped, so a raw newline never occurs
 * inside a chunk.
 *
 * The walk always advances, so a malformed body cannot spin: a zero-length
 * prefix, and a prefix too short to hold anything, both move past one line.
 */
function walkFrames(body: string): FrameWalk {
  const frames: Frame[] = [];
  let undecodedFrameCount = 0;
  // Whether anything at all looked like this envelope. A body of plain prose is
  // not "a response whose frames failed to decode" — it is not this protocol,
  // and the two must not be reported as the same finding.
  let sawEnvelopeShape = false;
  let pos = 0;

  while (pos < body.length) {
    // Leading separators between frames are not part of any chunk.
    while (pos < body.length && isSeparator(body.charAt(pos))) pos++;
    if (pos >= body.length) break;

    const headerEnd = body.indexOf('\n', pos);
    const header = headerEnd < 0 ? body.slice(pos) : body.slice(pos, headerEnd);
    const trimmedHeader = header.trim();
    const declared = /^\d+$/.test(trimmedHeader) ? Number(trimmedHeader) : null;
    if (declared !== null) sawEnvelopeShape = true;

    if (declared === null) {
      // No length line: the line itself has to be the chunk. Only an *array*
      // counts as envelope-shaped; a bare JSON object is some other document
      // (an error body, most likely) wearing the same transport.
      const end = headerEnd < 0 ? body.length : headerEnd;
      const chunk = body.slice(pos, end);
      const value = tryParseJson(chunk);
      if (value === NO_JSON) undecodedFrameCount++;
      else {
        if (Array.isArray(value)) sawEnvelopeShape = true;
        frames.push({ value, declaredLength: null, actualLength: chunk.length });
      }
      pos = end + 1;
      continue;
    }

    const start = headerEnd + 1;
    if (start >= body.length) {
      // A length line with nothing after it declares a chunk that is not there.
      undecodedFrameCount++;
      break;
    }

    const lineEnd = body.indexOf('\n', start);
    const lineChunk = lineEnd < 0 ? body.slice(start) : body.slice(start, lineEnd);
    const nextPos = lineEnd < 0 ? body.length : lineEnd + 1;

    // A zero-length prefix can never be a chunk; step past the line so the walk
    // terminates instead of re-reading the same position.
    if (declared === 0) {
      undecodedFrameCount++;
      pos = nextPos;
      continue;
    }

    const declaredEnd = start + declared;
    const declaredChunk = body.slice(start, declaredEnd);
    const declaredValue =
      declaredEnd >= body.length || isSeparator(body.charAt(declaredEnd))
        ? tryParseJson(declaredChunk)
        : NO_JSON;
    if (declaredValue !== NO_JSON) {
      frames.push({ value: declaredValue, declaredLength: declared, actualLength: declaredChunk.length });
      pos = start + declared;
      continue;
    }

    const lineValue = tryParseJson(lineChunk);
    if (lineValue === NO_JSON) undecodedFrameCount++;
    else frames.push({ value: lineValue, declaredLength: declared, actualLength: lineChunk.length });
    pos = nextPos;
  }

  return { frames, undecodedFrameCount, sawEnvelopeShape };
}

function isSeparator(ch: string): boolean {
  return ch === '\n' || ch === '\r' || ch === '\t' || ch === ' ';
}

/** Sentinel distinguishing "the text is not JSON" from a decoded JSON `null`. */
const NO_JSON = Symbol('no-json');

function tryParseJson(text: string): unknown {
  if (text.length === 0) return NO_JSON;
  try {
    return JSON.parse(text);
  } catch {
    return NO_JSON;
  }
}

// --- The envelope -----------------------------------------------------------

/**
 * Parses a raw `batchexecute` response body.
 *
 * Never throws: anything malformed comes back as `{ ok: false, reason }`, and
 * anything half-readable comes back as `ok: true` with the unreadable parts
 * *counted* rather than dropped. See `BatchExecuteParseResult` for what the
 * counts mean; in particular a response with no recognisable frame is a
 * failure, not a response with zero entries.
 */
export function parseBatchExecute(text: string): BatchExecuteParseResult {
  if (typeof text !== 'string') return { ok: false, reason: 'not-a-string' };
  if (text.trim().length === 0) return { ok: false, reason: 'empty-response' };

  const { body, guardPresent } = stripGuard(text);
  const { frames, undecodedFrameCount, sawEnvelopeShape } = walkFrames(body);
  // Nothing looked like this envelope at all — no length line and no JSON array
  // line. A body that happens to decode as a JSON *object* lands here too: that
  // is some other document wearing the same transport, most likely an error
  // body, and reporting it as "a response carrying no entries" would be exactly
  // the conflation this parser exists to prevent.
  if (!sawEnvelopeShape) return { ok: false, reason: 'not-a-batchexecute-body' };
  // Envelope-shaped positions were found, and not one of them decoded.
  if (frames.length === 0) return { ok: false, reason: 'frames-undecodable' };

  const entries: BatchExecuteEntry[] = [];
  let nonEntryArrayFrameCount = 0;
  let lengthPrefixMismatchCount = 0;

  for (const frame of frames) {
    if (frame.declaredLength !== null && frame.declaredLength !== frame.actualLength) {
      lengthPrefixMismatchCount++;
    }
    if (!Array.isArray(frame.value)) {
      nonEntryArrayFrameCount++;
      continue;
    }
    for (const raw of frame.value) {
      entries.push(toEntry(raw));
    }
  }

  return {
    ok: true,
    guardPresent,
    entries,
    undecodedFrameCount,
    nonEntryArrayFrameCount,
    lengthPrefixMismatchCount,
    unknownEntryCount: entries.filter((e) => !e.recognized).length,
  };
}

/**
 * Removes the `)]}'` anti-JSON guard.
 *
 * A missing guard is *not* a parse failure: a body without it is still readable,
 * and refusing it would turn "the guard was not there" into "we could not read
 * this". The caller is told which of the two happened instead.
 */
function stripGuard(text: string): { body: string; guardPresent: boolean } {
  let i = 0;
  // A byte-order mark or leading whitespace may precede the guard.
  while (i < text.length && (text.charAt(i) === '\uFEFF' || /\s/.test(text.charAt(i)))) i++;
  if (text.startsWith(GUARD, i)) return { body: text.slice(i + GUARD.length), guardPresent: true };
  return { body: text.slice(i), guardPresent: false };
}

/**
 * Reads one frame element into an entry.
 *
 * Only `wrb.fr` is a kind this parser claims to understand. Every other label —
 * including ones nobody has seen before — is kept verbatim with
 * `recognized: false` and counted, because a kind that gets dropped silently is
 * indistinguishable from a kind that arrived and carried nothing.
 */
function toEntry(raw: unknown): BatchExecuteEntry {
  if (!Array.isArray(raw)) {
    return { label: null, rpcid: null, payload: null, recognized: false, raw };
  }

  const label = typeof raw[0] === 'string' ? raw[0] : null;

  if (label !== 'wrb.fr') {
    return { label, rpcid: null, payload: null, recognized: false, raw };
  }

  const rpcid = typeof raw[1] === 'string' ? raw[1] : null;
  if (rpcid === null) {
    return { label, rpcid: null, payload: null, error: 'rpcid-absent', recognized: true, raw };
  }

  const inner = raw[2];
  if (typeof inner !== 'string') {
    return { label, rpcid, payload: null, error: 'inner-payload-absent', recognized: true, raw };
  }

  const decoded = tryParseJson(inner);
  if (decoded === NO_JSON) {
    return { label, rpcid, payload: null, error: 'inner-payload-not-json', recognized: true, raw };
  }

  return { label, rpcid, payload: decoded, recognized: true, raw };
}

// --- The request body -------------------------------------------------------

/**
 * Builds the form-encoded body the page posts: `f.req=<batch>&at=<xsrf token>`.
 *
 * The batch is a three-level nest whose middle level holds the RPC id and whose
 * inner document is a **JSON string**, not an object — `[[[rpcid, "<json>",
 * null, "generic"]]]`. Getting that wrong is the classic way this call 400s, so
 * the nesting is written out rather than assembled from pieces.
 *
 * Both fields are `encodeURIComponent`-encoded, matching the sources that build
 * this body by hand. (One source instead appends a trailing `&`; that is
 * accepted by the server but is not reproduced here. One other builds the body
 * with a form-serialiser, which would encode a space as `+`; the hand-built
 * form encodes it as `%20`. Both are valid for this content type, and the
 * hand-built form is the one three of the four agree on.)
 */
export function buildBatchExecuteBody(rpcid: string, args: unknown, at: string): string {
  const batch = [[[rpcid, JSON.stringify(args), null, 'generic']]];
  return `f.req=${encodeURIComponent(JSON.stringify(batch))}&at=${encodeURIComponent(at)}`;
}

// --- Positional readers for the two payloads --------------------------------
//
// Everything below navigates arrays by position, because these payloads carry no
// field names at all. Each index is named and each comment states how strong its
// evidence is. "Verified live" in a source's own words is still *one source's*
// claim, and the constant says so.

/**
 * `payload[1]` — the continuation token.
 * Evidence: sources agree. Absent on the last page, which is one of the two
 * end-of-list signals; the other is an empty item array.
 */
const LIST_NEXT_TOKEN_INDEX = 1;

/**
 * `payload[2]` — the array of conversation entries.
 * Evidence: sources agree on the position; the surrounding `[null, token, […]]`
 * shape is the same triple in both.
 */
const LIST_ITEMS_INDEX = 2;

/** `item[0]` — the conversation id, `c_`-prefixed. Evidence: sources agree. */
const LIST_ITEM_ID_INDEX = 0;

/**
 * `item[1]` — the conversation title.
 * Evidence: one source only. The detail payload carries no title, so this is
 * the only place a title exists.
 */
const LIST_ITEM_TITLE_INDEX = 1;

/**
 * `item[5]` — `[seconds, nanos]` creation time.
 * Evidence: one source only. Read as a two-element tuple; the seconds element
 * is what is used.
 */
const LIST_ITEM_CREATED_INDEX = 5;

/** `payload[0]` — the array of turns. Evidence: sources agree. */
const DETAIL_TURNS_INDEX = 0;

/**
 * `payload[1]` — the token into *older* turns, mirroring the list RPC.
 * Evidence: one source only for the detail RPC specifically.
 */
const DETAIL_NEXT_TOKEN_INDEX = 1;

/**
 * `turn[0]` — `[conversationId, responseId]`.
 * Evidence: sources agree on the pair and on its position in a turn.
 */
const DETAIL_TURN_IDS_INDEX = 0;

/** `turn[0][0]` — the conversation id. Evidence: sources agree. */
const DETAIL_TURN_CONVERSATION_ID_INDEX = 0;

/**
 * `turn[0][1]` — the response id. This is the dedupe key: pages arrive
 * newest-first and overlap, so turns are identified by this, not by position.
 * Evidence: sources agree.
 */
const DETAIL_TURN_RESPONSE_ID_INDEX = 1;

/**
 * `turn[4]` — `[seconds, nanos]` timestamp of the exchange. Evidence: sources
 * agree that the timestamp is at this position.
 */
const DETAIL_TURN_TIME_INDEX = 4;

/** Named reasons a payload did not have the shape the readers require. */
export type GeminiShapeError =
  | 'payload-not-an-array'
  | 'list-items-path-missing'
  | 'detail-turns-path-missing';

export interface GeminiListEntry {
  /**
   * `item[0]`, or null when the item carried none.
   *
   * Every item the page carried becomes an entry, including one whose id is
   * absent or empty: dropping it here would make "the page listed an item we
   * cannot address" indistinguishable from "the page listed nothing", and the
   * second of those is how a caller decides the account is fully enumerated.
   */
  id: string | null;
  /** `item[1]`, or null when the item carried none. An empty title stays empty; it is not null. */
  title: string | null;
  /** `item[5][0]` as epoch seconds, or null when the item carried none. */
  createdAtSeconds: number | null;
}

export interface GeminiListPage {
  conversations: GeminiListEntry[];
  /**
   * The token for the next page, or null when the response carried none.
   *
   * Null means "the server did not send one", which is one of the two
   * end-of-list signals; it does not mean the token was empty, which is why an
   * empty string is reported as an empty string. `pageIsEmpty` is reported
   * separately because the sources stop on *either* signal, and collapsing the
   * two here would lose which one fired.
   */
  nextPageToken: string | null;
  /** True when the page carried zero items. A measurement, not a fallback. */
  pageIsEmpty: boolean;
}

export interface GeminiTurnKey {
  /** `turn[0][0]`. */
  conversationId: string | null;
  /** `turn[0][1]` — the dedupe key across pages. */
  responseId: string | null;
  /** `turn[4][0]` as epoch seconds, or null when the turn carried none. */
  timestampSeconds: number | null;
  /** `turn[4][1]` as nanoseconds, or null. Kept so callers can order sub-second turns. */
  timestampNanos: number | null;
}

export interface GeminiDetailPage {
  turns: GeminiTurnKey[];
  /** The token into older turns, or null when the response carried none. */
  nextPageToken: string | null;
  /** True when the page carried zero turns. A measurement, not a fallback. */
  pageIsEmpty: boolean;
}

export type GeminiListPageResult =
  | ({ ok: true } & GeminiListPage)
  | { ok: false; reason: GeminiShapeError };

export type GeminiDetailPageResult =
  | ({ ok: true } & GeminiDetailPage)
  | { ok: false; reason: GeminiShapeError };

/**
 * Reads one `MaZiqc` page.
 *
 * A missing or non-array items path is `list-items-path-missing`, never an empty
 * list: "this page listed nothing" and "this payload is not a list page" are
 * different findings, and the caller has to be able to tell them apart before it
 * decides the account was fully enumerated.
 */
export function extractListPage(payload: unknown): GeminiListPageResult {
  if (!Array.isArray(payload)) return { ok: false, reason: 'payload-not-an-array' };

  const rawItems = payload[LIST_ITEMS_INDEX];
  if (!Array.isArray(rawItems)) return { ok: false, reason: 'list-items-path-missing' };

  const conversations: GeminiListEntry[] = [];
  for (const item of rawItems) {
    conversations.push({
      id: arrayString(item, LIST_ITEM_ID_INDEX),
      title: arrayString(item, LIST_ITEM_TITLE_INDEX),
      createdAtSeconds: arraySeconds(item, LIST_ITEM_CREATED_INDEX),
    });
  }

  return {
    ok: true,
    conversations,
    nextPageToken: arrayString(payload, LIST_NEXT_TOKEN_INDEX),
    pageIsEmpty: conversations.length === 0,
  };
}

/**
 * Reads one `hNvQHb` page.
 *
 * The detail payload carries no title (titles live only in the list), so none is
 * invented here. Turns arrive newest-first and are returned in the order the
 * response gave them; ordering and reversal are the caller's business, and doing
 * it here would hide which order the response actually used.
 */
export function extractDetailPage(payload: unknown): GeminiDetailPageResult {
  if (!Array.isArray(payload)) return { ok: false, reason: 'payload-not-an-array' };

  const rawTurns = payload[DETAIL_TURNS_INDEX];
  if (!Array.isArray(rawTurns)) return { ok: false, reason: 'detail-turns-path-missing' };

  const turns: GeminiTurnKey[] = [];
  for (const turn of rawTurns) {
    const ids = arrayValue(turn, DETAIL_TURN_IDS_INDEX);
    turns.push({
      conversationId: arrayString(ids, DETAIL_TURN_CONVERSATION_ID_INDEX),
      responseId: arrayString(ids, DETAIL_TURN_RESPONSE_ID_INDEX),
      timestampSeconds: arraySeconds(turn, DETAIL_TURN_TIME_INDEX),
      timestampNanos: arrayIndex(turn, DETAIL_TURN_TIME_INDEX, 1),
    });
  }

  return {
    ok: true,
    turns,
    nextPageToken: arrayString(payload, DETAIL_NEXT_TOKEN_INDEX),
    pageIsEmpty: turns.length === 0,
  };
}

// --- Positional accessors ---------------------------------------------------
//
// These return null for "the position held nothing readable". They never coerce,
// so a number cannot become a string and a missing value cannot become zero.

function arrayValue(value: unknown, index: number): unknown {
  return Array.isArray(value) ? value[index] : undefined;
}

function arrayIndex(value: unknown, index: number, subIndex: number): number | null {
  const at = arrayValue(arrayValue(value, index), subIndex);
  return typeof at === 'number' && Number.isFinite(at) ? at : null;
}

/**
 * The string at `index`, or null when the position held something else.
 *
 * Note what this does *not* do: it does not fold an empty string into null. A
 * field that arrived empty and a field that did not arrive are different facts,
 * and a reader that merged them would be reporting an unknown as a measurement.
 * Whether an empty value is *usable* is the caller's judgement, not this
 * accessor's.
 */
function arrayString(value: unknown, index: number): string | null {
  const at = arrayValue(value, index);
  return typeof at === 'string' ? at : null;
}

/**
 * The seconds element of a `[seconds, nanos]` tuple at `index`, or null.
 * A bare number at `index` is accepted too, because the tuple nesting is one of
 * the weaker claims in the evidence table.
 */
function arraySeconds(value: unknown, index: number): number | null {
  const at = arrayValue(value, index);
  const seconds = Array.isArray(at) ? at[0] : at;
  return typeof seconds === 'number' && Number.isFinite(seconds) ? seconds : null;
}

// --- 🔴 W29 · Reading one RPC's document out of a whole response -------------
//
// Everything below is what the capture and backfill legs actually call. Each
// reader folds `parseBatchExecute` -> "the entry for this rpcid" -> that
// payload's own positional reader, and reports a **named reason** at every step
// instead of returning an empty-looking result. That is the property that makes
// "we could not read this response" and "this response held nothing" different
// facts all the way up: the callers halt on one and archive the other.

/**
 * Named reasons a response carried no readable document for one rpcid.
 *
 * `rpcid-absent` and `payload-unusable` are deliberately separate: the first is
 * "this response is not the RPC we asked for", the second is "it is, and the
 * document inside it could not be read". A caller that merged them could not
 * tell a wrong-rpcid response from a corrupted one.
 */
export type GeminiRpcPayloadFailure =
  | BatchExecuteFailureReason
  /** No `wrb.fr` entry named this rpcid anywhere in the response. */
  | 'rpcid-absent'
  /** The entry is there and its inner document is null or did not parse. */
  | 'payload-unusable';

export type GeminiRpcPayloadResult =
  | { ok: true; payload: unknown }
  | { ok: false; reason: GeminiRpcPayloadFailure };

/**
 * The decoded document of one rpcid, or a named reason.
 *
 * Only entries of the kind this parser understands (`wrb.fr`) are considered: an
 * entry of another kind carries no rpcid here (see `toEntry`), so it can neither
 * be matched nor mistaken for a document.
 */
export function selectRpcPayload(
  result: BatchExecuteParseResult,
  rpcid: string,
): GeminiRpcPayloadResult {
  if (!result.ok) return { ok: false, reason: result.reason };
  for (const entry of result.entries) {
    if (!entry.recognized || entry.rpcid !== rpcid) continue;
    if (entry.error !== undefined) return { ok: false, reason: 'payload-unusable' };
    return { ok: true, payload: entry.payload };
  }
  return { ok: false, reason: 'rpcid-absent' };
}

/** One `MaZiqc` page, as the capture and backfill legs consume it. */
export interface GeminiListRead {
  /** `item[0]` of every item, **in the order the page gave them**, `c_` prefix included. */
  ids: string[];
  /** `payload[1]`, or null when the response carried none. */
  nextPageToken: string | null;
  /** True when the page carried zero items. A measurement, not a fallback. */
  pageIsEmpty: boolean;
}

export type GeminiListReadResult =
  | ({ ok: true } & GeminiListRead)
  | { ok: false; reason: GeminiRpcPayloadFailure | GeminiShapeError | 'item-id-missing' };

/** Reads one `MaZiqc` response body: envelope, then this rpcid's document, then one page of it. */
export function readListResponse(text: string): GeminiListReadResult {
  const selected = selectRpcPayload(parseBatchExecute(text), GEMINI_RPC_LIST);
  if (!selected.ok) return selected;
  const page = extractListPage(selected.payload);
  if (!page.ok) return page;
  const ids: string[] = [];
  for (const conversation of page.conversations) {
    // 🔴 An item whose id is not a readable string is a **shape error**, not a
    //    skip: this page listed a conversation this code cannot address, and
    //    passing the rest on would lose it while the leg reported success. Same
    //    rule as parseKimiListPage's unclassifiable feed item. (An id that is
    //    present and empty lands here too — an empty string is not an
    //    addressable conversation id.)
    if (conversation.id === null || conversation.id.length === 0) {
      return { ok: false, reason: 'item-id-missing' };
    }
    ids.push(conversation.id);
  }
  return {
    ok: true,
    ids,
    nextPageToken: page.nextPageToken,
    pageIsEmpty: page.pageIsEmpty,
  };
}

/** One `hNvQHb` page, as the capture and backfill legs consume it. */
export interface GeminiDetailRead {
  /**
   * The conversation id the page's turns name, or null when no turn named one.
   * Taken from `turn[0][0]` — the canonical (`c_`-prefixed) form, the same value
   * the list endpoint returns, which is what makes a live capture and a backfill
   * debt land on one identity.
   */
  conversationId: string | null;
  /**
   * `turn[0][1]` of every turn, in the order given — the dedupe key across
   * pages, and the evidence that a later page really is a later page.
   */
  responseIds: string[];
  /** True when the page carried zero turns. */
  pageIsEmpty: boolean;
  /** `payload[1]`, or null when the response carried none — the end of the conversation. */
  nextPageToken: string | null;
}

export type GeminiDetailReadFailure =
  | GeminiRpcPayloadFailure
  | GeminiShapeError
  /** A turn on this page named a different conversation than the caller asked for. */
  | 'conversation-id-mismatch';

export type GeminiDetailReadResult =
  | ({ ok: true } & GeminiDetailRead)
  | { ok: false; reason: GeminiDetailReadFailure };

/**
 * Reads one `hNvQHb` response body.
 *
 * `expectedConversationId` is checked, not assumed: every turn whose
 * `turn[0][0]` is readable must name it. That is the one integrity property the
 * multi-page loop depends on — without it, a page belonging to another
 * conversation (a stale token, a redirect) would be concatenated into this
 * conversation's bundle and nothing would say so.
 */
export function readDetailResponse(
  text: string,
  expectedConversationId?: string,
): GeminiDetailReadResult {
  const selected = selectRpcPayload(parseBatchExecute(text), GEMINI_RPC_DETAIL);
  if (!selected.ok) return selected;
  const page = extractDetailPage(selected.payload);
  if (!page.ok) return page;

  const responseIds: string[] = [];
  let conversationId: string | null = null;
  for (const turn of page.turns) {
    if (turn.conversationId !== null) {
      if (expectedConversationId !== undefined && turn.conversationId !== expectedConversationId) {
        return { ok: false, reason: 'conversation-id-mismatch' };
      }
      conversationId ??= turn.conversationId;
    }
    if (turn.responseId !== null) responseIds.push(turn.responseId);
  }

  return {
    ok: true,
    conversationId,
    responseIds,
    pageIsEmpty: page.pageIsEmpty,
    nextPageToken: page.nextPageToken,
  };
}

// --- 🔴 W29 · The bundle: every raw page of one conversation, in order -------

/**
 * The JSON document the archive holds for one Gemini conversation.
 *
 * ## Why the archive holds a wrapper here and a raw body everywhere else
 * Every other platform's body is **one** response, so its raw text is the whole
 * artefact and is stored verbatim. A Gemini conversation is not one response: it
 * is a first page plus every page the continuation token led to, and no single
 * response is the conversation. Storing only the first page would archive a
 * partial conversation as a complete one — the failure this repository's first
 * invariant exists to prevent — so the archived artefact is the **sequence of
 * raw pages**, verbatim and in order, with the two facts needed to read them
 * back (which RPC produced them, and which conversation they are).
 *
 * 🔴 Nothing inside `pages` is trimmed, reordered, re-serialised or deduplicated.
 *    Pages measured on 2026-09-14 were disjoint, but overlap is what the sources
 *    expect and dedupe is a consumer's business, not this function's: a raw body
 *    that has been through a normaliser is no longer raw.
 */
export interface GeminiDetailBundle {
  /** The rpcid every page in `pages` answered. Always `GEMINI_RPC_DETAIL` today. */
  rpcid: string;
  /** `c_`-prefixed conversation id — the canonical form, and the identity the file name comes from. */
  conversationId: string;
  /** Every raw page body, exactly as the server sent it, oldest page first. */
  pages: string[];
}

/** Builds the bundle document. The only place this wrapper's shape is written. */
export function assembleDetailBundle(conversationId: string, pages: readonly string[]): string {
  const bundle: GeminiDetailBundle = {
    rpcid: GEMINI_RPC_DETAIL,
    conversationId,
    pages: [...pages],
  };
  return JSON.stringify(bundle);
}

export type GeminiBundleReadResult =
  | { ok: true; bundle: GeminiDetailBundle }
  | { ok: false; reason: string };

/**
 * Reads a bundle back and re-establishes the property it claims: **this is a
 * whole conversation**.
 *
 * Three checks, and each one is a different way the claim could be false:
 *  · the wrapper itself (`rpcid`, a non-empty `conversationId`, a non-empty
 *    array of strings);
 *  · every page parses as this platform's detail response **for this same
 *    conversation** — a page naming another conversation is refused;
 *  · **the token is where it should be and nowhere else.** Every page except the
 *    last must carry a continuation token — a page in the middle that says
 *    "no more" means these pages do not belong together — and the **last** page
 *    must carry none. A bundle whose last page still has a token is a truncated
 *    conversation wearing a complete one's name, which is the one thing this
 *    format exists to make impossible, and it is reported rather than read as
 *    fine.
 *
 * An empty-but-tokenless last page is accepted: from one response, "this
 * conversation has no turns" and "this response is a window with nothing in it"
 * are not distinguishable, and refusing it would leave a debt pending forever.
 * (The window case is what the last-page token check above is for — see
 * parseKimiDetailPage for the same trade-off written out.)
 */
export function readDetailBundle(text: string): GeminiBundleReadResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, reason: 'bundle is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, reason: 'bundle is not a JSON object' };
  }
  const record = body as Record<string, unknown>;
  if (typeof record.rpcid !== 'string' || record.rpcid.length === 0) {
    return { ok: false, reason: 'bundle has no rpcid' };
  }
  if (typeof record.conversationId !== 'string' || record.conversationId.length === 0) {
    return { ok: false, reason: 'bundle has no conversationId' };
  }
  const pages = record.pages;
  if (!Array.isArray(pages) || pages.length === 0) {
    return { ok: false, reason: 'bundle carries no pages' };
  }
  for (const [index, page] of pages.entries()) {
    if (typeof page !== 'string' || page.length === 0) {
      return { ok: false, reason: 'bundle holds a page that is not a non-empty string' };
    }
    const read = readDetailResponse(page, record.conversationId);
    if (!read.ok) return { ok: false, reason: `bundle page could not be read: ${read.reason}` };
    const isLast = index === pages.length - 1;
    if (isLast && read.nextPageToken !== null) {
      return { ok: false, reason: 'the bundle’s last page carries a continuation token, so the bundle is incomplete' };
    }
    if (!isLast && read.nextPageToken === null) {
      return { ok: false, reason: 'a page before the last one carries no continuation token' };
    }
  }
  return {
    ok: true,
    bundle: { rpcid: record.rpcid, conversationId: record.conversationId, pages: pages as string[] },
  };
}
