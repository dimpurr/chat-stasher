/**
 * C20 · The list of conversations that could not be stored.
 *
 * The product owner's own words, kept here so they do not get lost:
 *   **"Losing something" and "losing something but knowing about it" are two
 *     entirely different things, and this project exists for the second one.**
 *
 * So this list is not a "retry queue" — it is **a receipt meant to be read by a
 * person**: which ones I failed to store, roughly what kind of problem it was,
 * and when it happened.
 *
 * 🔴 Four hard rules:
 *  1. **No retrying.** That is a product decision. There is **no** function in
 *     this file that puts an entry back into the debt set, and the engine never
 *     reads this list to decide what to fetch. Its only exits are "show it to a
 *     human" and "clear it".
 *  2. **No unbounded growth.** See the cap and its argument on MAX_FAILURES; the
 *     overflow policy is "drop the oldest", but how many were dropped must be
 *     recorded in `dropped` — 🔴 silent truncation would turn the list itself
 *     into a lie.
 *  3. **Store only what can locate the problem.** Short conversation id (first
 *     8 characters) / reason code / timestamp / platform id. 🔴 Never the
 *     conversation body, never a full URL (a URL may carry a token), and never
 *     the complete session id.
 *  4. **A reason code must not guess a cause** (the rule carried over from
 *     C12). Every code states only a fact **we observed ourselves** ("I could
 *     not extract a session id" is a fact; "the platform changed its API" is a
 *     guess), and an unrecognised code is shown verbatim rather than dressed up
 *     in a plausible sentence.
 *
 * Where it lives: **inside BackfillState** (`state.failures` /
 * `state.failuresDropped`). 🔴 Deliberately not a storage key of its own — the
 * root cause of the defect this came from was "the same identity expressed
 * twice", and creating a second ledger of "what happened to this conversation"
 * would be the same mistake again. Debts, archived, failures: three columns of
 * one ledger.
 */

import { t } from '../i18n';
import type { BackfillState } from './types';

/** How many characters of a session id are kept. 8 identifies it in a log line and cannot be reversed. */
export const SHORT_ID_LEN = 8;

/**
 * 🔴 The cap is 50. Why 50:
 *
 *  · It is **for a person to read**, not a database. 50 entries are already far
 *    more than anyone will scan in a popup; more adds no actionable information.
 *  · Past a handful, failures stop being "one conversation is special" and
 *    become **systemic** (the API changed / the directory is not writable). At
 *    that point the 51st id carries zero information over the 50th — what you
 *    want to look at is the reason code, not the length of the list.
 *  · Check it against the runtime dimensions: the daily cap on fetching bodies
 *    is 200 (`lib/backfill/pace.ts`, DEFAULT_DETAIL_PACE.maxPerDay = 200).
 *    50 = a quarter of one day's theoretical throughput. If a quarter of a day's
 *    work failed, the conclusion is already in.
 *  · The storage cost is bounded along the way: 50 × ~80 bytes ≈ 4 KiB, which is
 *    nothing next to the debt set itself, so "the failure list blows up
 *    storage" does not become a new failure mode.
 *
 * Overflow policy: **drop the oldest** (not "stop recording"). Reason: the
 * newest failures are the ones that describe the current problem; stopping
 * would freeze the list on a stale snapshot, which misleads more than dropping
 * old rows does. The number dropped goes into `failuresDropped`, and the
 * wording says plainly "N older one(s) are no longer on it".
 */
export const MAX_FAILURES = 50;

/**
 * Reason codes. A **closed set**, each corresponding to one definite branch in
 * the engine. 🔴 A new code must arrive together with a sentence in
 * `describeFailureReason` that states an observed fact and nothing more.
 */
export type FailureReason =
  /** The sink answered saved:false outright — not one byte went out. */
  | 'not-saved'
  /**
   * The sink said the write succeeded, but the identity it named it by does not
   * match the debt key.
   * 🔴 This one points straight at the root cause: the debt key came from the
   *    list API's items[].id, while the file name came from "scrape it out of
   *    the URL a second time", and nothing checked the two against each other in
   *    between — so two different debts could collapse onto the same file name
   *    and the later write would overwrite the earlier one (C17 BUG-2). Now a
   *    mismatch leaves the debt open.
   */
  | 'identity-mismatch'
  /**
   * 🔴 W22 · The body was fetched, HTTP succeeded, the shape was recognised — and
   * the response itself says it is **not the whole conversation** (Kimi's detail
   * response carried a non-empty next-page token, and this leg does not page that
   * endpoint).
   *
   * A fact we observed, not a diagnosis: it says "this response declared more
   * content than it held". Deliberately **not** phrased as "the platform changed
   * its API" (that is a guess) and not as an error (there was no error) — and it
   * is deliberately not 'not-saved', which would say the write failed when the
   * truth is that there was nothing whole to write.
   *
   * 🔴 Why a truncated body is a failure and not a stored conversation: archiving
   *    it would put a partial answer in the archive with nothing marking it as
   *    partial, and settle a debt for a conversation that was never captured. The
   *    failure list is exactly the place built for "this one is missing, and here
   *    is why" (see the file header).
   */
  | 'detail-paged-unsupported'
  /**
   * 🔴 W29 · The body was fetched, HTTP succeeded, the shape was recognised — and
   * the conversation needs **more pages than this leg will fetch** (Gemini: the
   * detail RPC pages with a continuation token, and the cap is
   * `DetailPagesSpec.maxPages`).
   *
   * 🔴 Why this is a *different* fact from 'detail-paged-unsupported', and why the
   *    two must not be merged: that one says "the platform offers more of this
   *    conversation and this leg does not know how to ask for it"; this one says
   *    "this leg does know how, did ask, and stopped itself at its own cap". The
   *    first is a gap in what we implemented — it applies to every long
   *    conversation on that platform. The second is a property of **this one
   *    conversation**: every shorter conversation beside it is archived in the
   *    same run. Merging them would either halt a leg that is working or describe
   *    a per-conversation limit as a missing capability.
   *
   * The refusal is the point: the pages fetched so far are real content and
   * **still not the conversation**, so nothing is stored and the debt leaves
   * pending with this receipt — never "the first N pages, called complete".
   */
  | 'detail-too-long'
  /**
   * 🔴 W31 · The body was fetched, HTTP succeeded, the shape was recognised — and
   * **the response's own parent links do not reach a root**. Two platforms reach
   * this reason, and both are trees with a named current leaf: claude.ai (the
   * chain from `current_leaf_message_uuid` upward hits a message the response does
   * not carry) and, since 🔴 W42, DeepSeek (the chain from
   * `chat_session.current_message_id` upward along `parent_id` leaves the messages
   * the response carries, revisits one, or starts at a leaf the response does not
   * hold).
   *
   * A fact we observed, phrased as one: it says "walking back from the leaf left
   * the messages this response holds". It is deliberately **not** phrased as "the
   * platform truncated your conversation" — that is a guess about *why* a parent
   * is absent, and the open question the research records is exactly whether a
   * long conversation is capped server-side. It is also not 'shape-changed': the
   * shape is precisely what the platform row describes.
   *
   * 🔴 What it is **not** used for: a body whose tree pointers are absent entirely.
   *    There the plan's parser answers `{ok:false}` and the leg halts
   *    'shape-changed', because "this conversation is partial" is not a statement
   *    this code may make about a conversation whose branch it never walked.
   *
   * 🔴 Why an incomplete tree is a failure and not a stored conversation: this is
   *    the same reasoning as 'detail-paged-unsupported' above. The archive would
   *    hold a partial conversation with nothing marking it partial — and the
   *    specific loss here is the *middle* of the branch, which no reader could
   *    even notice.
   */
  | 'detail-tree-incomplete';

export interface FailureEntry {
  /** The first 8 characters of the session id. 🔴 Not the full id. */
  shortId: string;
  /** Platform id (chatgpt / claude / ...). Not sensitive, and without it you do not know where to look. */
  platform: string;
  reason: FailureReason | string;
  /** clock.now() milliseconds. */
  at: number;
}

/** Keep the first 8 characters only. An empty id ⇒ "(empty)", never a pretended one. */
export function shortIdOf(id: string): string {
  if (!id) return t('failure.shortIdEmpty');
  return id.slice(0, SHORT_ID_LEN);
}

/**
 * The list read back. Older state objects without these fields ⇒ empty,
 * byte-identical to C19 behaviour.
 *
 * 🔴 W18 · The parameter is the *failure-carrying* part of the state, not a whole
 *    `BackfillState`: the popup reaches this through the persisted header
 *    (`BackfillHeader`), which has no debt ids. Both types carry `failures` and
 *    `failuresDropped`, and naming exactly that here is what lets one function
 *    serve both without either side pretending to be the other.
 */
/** The failure list's two fields, wherever they live: the in-memory state or the persisted header. */
export type FailureCarrier = Pick<BackfillState, 'failures' | 'failuresDropped'>;

export function failuresOf(state: FailureCarrier): FailureEntry[] {
  return Array.isArray(state.failures) ? state.failures : [];
}

export function droppedOf(state: FailureCarrier): number {
  return typeof state.failuresDropped === 'number' && state.failuresDropped > 0
    ? state.failuresDropped
    : 0;
}

/**
 * Record one failure. **Mutates `state` in place** (same style as settleDebt;
 * the caller's persist is what writes it to disk).
 *
 * 🔴 This function does **not** put the id back into pending, and does **not**
 *    write it into archived: no retry (product decision), and never a pretence
 *    of "archived". From here on it lives only on this list.
 *
 * 🔴 "It will not be re-queued" is **structural**, not enforced by a blocklist:
 *    the engine's enumeration phase is `while (!state.enumCursor.complete)`, and
 *    the body-fetching phase only starts after enumeration finishes ⇒ by the
 *    time any failure can happen enumCursor.complete is already true ⇒ every
 *    later tick skips enumeration outright, so there is no path that
 *    "enumerates again and picks it back up". (Which is exactly why this does
 *    **not** need to keep the full session id for deduplication — rule 3 holds.)
 */
export function recordFailure(
  state: BackfillState,
  entry: { id: string; reason: FailureReason | string; at: number },
): FailureEntry {
  const row: FailureEntry = {
    shortId: shortIdOf(entry.id),
    platform: state.platform,
    reason: entry.reason,
    at: entry.at,
  };
  const list = failuresOf(state).slice();
  list.push(row);
  let dropped = droppedOf(state);
  while (list.length > MAX_FAILURES) {
    list.shift();
    dropped += 1;
  }
  state.failures = list;
  state.failuresDropped = dropped;
  return row;
}

/** The user's "acknowledge / clear". `dropped` is zeroed too — acknowledged is acknowledged. */
export function clearFailures(state: FailureCarrier): void {
  state.failures = [];
  state.failuresDropped = 0;
}

/**
 * Reason code → one sentence stating an observed fact.
 * 🔴 An unrecognised code is **returned unchanged**; never invent one.
 */
export function describeFailureReason(reason: string): string {
  switch (reason) {
    case 'not-saved':
      return t('failure.notSaved');
    case 'identity-mismatch':
      return t('failure.identityMismatch');
    case 'detail-paged-unsupported':
      return t('failure.detailPagedUnsupported');
    case 'detail-too-long':
      return t('failure.detailTooLong');
    case 'detail-tree-incomplete':
      return t('failure.detailTreeIncomplete');
    default:
      return t('failure.unknownReason', { reason });
  }
}
