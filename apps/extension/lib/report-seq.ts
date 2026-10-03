/**
 * 🔴 ADR-045 · **The per-instance report sequence — the only evidence that two
 * live writers share one install id.**
 *
 * Copying a browser profile copies `storage.local`, so the copy carries the
 * same `install_id`, the same account-fingerprint salt and the same profile
 * label. Nothing in that storage can tell the two apart, and the host has no
 * profile coordinate to ask for (Chromium native messaging hands it a caller
 * origin and, on Windows only, a window handle). What a copy cannot carry is
 * the original's *future*: each instance keeps its own counter, incremented for
 * every status report and every capture delivery, so two live writers on one id
 * eventually allocate the same number.
 *
 * Three rules make that signal mean what it says:
 *
 * ① **Unknown is `null`, never `0`.** A fabricated zero is indistinguishable
 *    from a writer that has never reported, which is exactly what a regression
 *    looks like — guessing one would accuse an honest install of being a copy.
 *    Unreadable or corrupt storage therefore produces no sequence at all, and
 *    the message goes out without the field, which the host accepts as "seq
 *    unknown".
 *
 * ② **A number is not evidence on its own; a number and its nonce are.** Two
 *    copies start from the same stored value and advance independently, so they
 *    produce the same numbers — and a number that repeats also arises from one
 *    writer's frame being replayed at the transport. What separates those is
 *    the `nonce`: a fresh random value minted *when the sequence is allocated*
 *    and persisted beside it, so every message carrying sequence *n* carries
 *    *n*'s nonce. A repeat of a pair the host has already recorded is one
 *    allocation arriving twice — an idempotent retry — while the same sequence
 *    under a **different** nonce is two writers that allocated the same number
 *    independently, which is the conflict. Readings are judged, and orderings
 *    are not: a report that arrives late, or out of order, or after a worker
 *    restart, carries a sequence of its own and is nobody's clone.
 *
 * ③ **The allocation is durable before it is used.** The pair is written, and
 *    read back, *before* the message that carries it is sent, and every
 *    allocation in this worker goes through one chain. A service worker that
 *    restarts between allocating and sending therefore resumes *past* the value
 *    it had already reserved rather than re-issuing it, and two concurrent sends
 *    cannot both read the same base.
 */
import { localStorageArea } from './local-storage';

export const REPORT_SEQ_KEY = 'cs_report_seq_v1';

/** One allocation: the sequence to send, and the nonce that identifies it. */
export interface ReportStamp {
  seq: number;
  nonce: string;
}

/** Three states, because "not there" and "not readable" must not merge. */
type StampRead =
  | { kind: 'absent' }
  | { kind: 'known'; seq: number; nonce: string | null }
  | { kind: 'unknown' };

/** The shape the extension ships to the host: `[A-Za-z0-9_-]{1,128}` (§6.1). */
const NONCE = /^[A-Za-z0-9_-]{1,128}$/;

async function readStamp(): Promise<StampRead> {
  const storage = localStorageArea();
  if (!storage) return { kind: 'unknown' };
  try {
    const found = await storage.get({ [REPORT_SEQ_KEY]: null });
    const saved = found[REPORT_SEQ_KEY];
    if (saved === undefined || saved === null) return { kind: 'absent' };
    // A bare number is the shape this key held before the nonce existed. The
    // sequence is a real reading and is kept; the nonce is *unknown*, which is
    // not a value — a stored sequence with no nonce is never compared against.
    if (typeof saved === 'number') {
      if (!Number.isSafeInteger(saved) || saved < 0) return { kind: 'unknown' };
      return { kind: 'known', seq: saved, nonce: null };
    }
    if (typeof saved !== 'object' || Array.isArray(saved)) return { kind: 'unknown' };
    const { seq, nonce } = saved as { seq?: unknown; nonce?: unknown };
    if (typeof seq !== 'number' || !Number.isSafeInteger(seq) || seq < 0) {
      return { kind: 'unknown' };
    }
    if (typeof nonce !== 'string' || !NONCE.test(nonce)) return { kind: 'unknown' };
    return { kind: 'known', seq, nonce };
  } catch {
    return { kind: 'unknown' };
  }
}

/** The sequence this instance last allocated, or `null` when that is not knowable. */
export async function readReportSeq(): Promise<number | null> {
  const read = await readStamp();
  return read.kind === 'known' ? read.seq : null;
}

/**
 * A nonce no other allocation can share.
 *
 * It has to be *random*, not derived: a value derived from the report (its
 * sequence, its profile, its bytes) would be minted identically by a copy — and
 * a copy is exactly what this is here to tell apart from the original.
 */
function newReportNonce(): string | null {
  try {
    if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
  } catch {
    return null;
  }
}

/**
 * Reserve the next sequence with a nonce of its own, persist the pair, and
 * return it — or `null` when no sequence can be stood behind.
 *
 * The write is verified by reading it back, exactly as `getInstallIdentity`
 * verifies its own record: `storage.local` has no compare-and-swap, so a value
 * this function cannot confirm is not a value it may stamp on a message. An
 * unconfirmed write returns `null` — "unknown" — instead of a number, and so
 * does an absent random source.
 */
async function allocateStamp(): Promise<ReportStamp | null> {
  const storage = localStorageArea();
  if (!storage) return null;
  const read = await readStamp();
  // 🔴 A corrupt stored value is *left alone*. Overwriting it would destroy the
  //    only evidence that this writer's sequence is unusable, and the field
  //    would then look healthy again on the next read.
  if (read.kind === 'unknown') return null;
  const seq = (read.kind === 'known' ? read.seq : 0) + 1;
  // The stored boundary value is valid, but it has no representable successor.
  // Do not persist a number that the next read would have to call corrupt.
  if (!Number.isSafeInteger(seq)) return null;
  const nonce = newReportNonce();
  if (nonce === null) return null;
  const stamp: ReportStamp = { seq, nonce };
  try {
    await storage.set({ [REPORT_SEQ_KEY]: stamp });
    const confirmed = await storage.get({ [REPORT_SEQ_KEY]: null });
    const saved = confirmed[REPORT_SEQ_KEY] as Partial<ReportStamp> | undefined;
    return saved?.seq === seq && saved.nonce === nonce ? stamp : null;
  } catch {
    return null;
  }
}

/**
 * Forget the sequence.
 *
 * Used by the repair only, and *after* the new install id is persisted: a reset
 * on the old id would look like a regression and would be evidence of the very
 * thing the repair is undoing. A reset that fails is not an error worth
 * surfacing — the sequence is keyed by nothing, and a fresh install id has no
 * recorded value for the host to compare against, so the stale number cannot
 * make the new identity look shared.
 */
export function resetReportSeq(): Promise<void> {
  return onChain(async () => {
    const storage = localStorageArea();
    if (!storage) return;
    try {
      await storage.set({ [REPORT_SEQ_KEY]: null });
    } catch {
      // Deliberately swallowed: see above. The new identity is already persisted.
    }
  });
}

/**
 * The one chain every allocation runs on (rule ③).
 *
 * A worker claims a single lease per report and per delivery, and native
 * messaging gives each message its own host process, so two sends in flight can
 * be *processed* in either order. Reserving and sending under one lock is what
 * makes the pair a statement about one allocation rather than about whichever
 * microtask ran first.
 */
let seqChain: Promise<unknown> = Promise.resolve();

function onChain<T>(task: () => Promise<T>): Promise<T> {
  const run = seqChain.then(task);
  seqChain = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

/**
 * The number this instance would send next, reserved and persisted.
 *
 * Exported for callers that need the value without sending anything (and for
 * the tests that pin monotonicity). It takes the same chain as
 * [`withReportSeq`], so a concurrent reservation cannot read the base another
 * reservation is about to overwrite.
 */
export function nextReportStamp(): Promise<ReportStamp | null> {
  return onChain(allocateStamp);
}

/** [`nextReportStamp`]'s sequence alone, or `null` when none could be reserved. */
export async function nextReportSeq(): Promise<number | null> {
  const stamp = await nextReportStamp();
  return stamp === null ? null : stamp.seq;
}

/**
 * Stamp and send, one at a time.
 *
 * The chain is shared by every sequence-bearing message, and it must survive a
 * rejected link: a send that fails, or that times out, has to release the next
 * message rather than wedge it. `send` receives `null` when no stamp could be
 * reserved, and the caller omits both fields in that case.
 */
export function withReportSeq<T>(send: (stamp: ReportStamp | null) => Promise<T>): Promise<T> {
  return onChain(async () => send(await allocateStamp()));
}
