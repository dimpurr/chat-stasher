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
 * eventually send a value at or below one already recorded.
 *
 * Two rules make that signal mean what it says:
 *
 * ① **Unknown is `null`, never `0`.** A fabricated zero is indistinguishable
 *    from a writer that has never reported, which is exactly what a regression
 *    looks like — guessing one would accuse an honest install of being a copy.
 *    Unreadable or corrupt storage therefore produces no sequence at all, and
 *    the message goes out without the field, which the host accepts as "seq
 *    unknown".
 *
 * ② **The values a single instance sends must reach the host in order.** The
 *    host can only see arrival order, and each native message is handled by its
 *    own host process, so two messages in flight at once can be processed in
 *    either order — and the later one would then look exactly like a copy. So
 *    stamping and sending happen under one lock (`withReportSeq`), which is what
 *    makes "at or below the high-water mark" evidence of a second writer rather
 *    than evidence of a slow one.
 */
import { localStorageArea } from './local-storage';

export const REPORT_SEQ_KEY = 'cs_report_seq_v1';

/** Three states, because "not there" and "not readable" must not merge. */
type SeqRead =
  | { kind: 'absent' }
  | { kind: 'known'; value: number }
  | { kind: 'unknown' };

async function readSeq(): Promise<SeqRead> {
  const storage = localStorageArea();
  if (!storage) return { kind: 'unknown' };
  try {
    const found = await storage.get({ [REPORT_SEQ_KEY]: null });
    const saved = found[REPORT_SEQ_KEY];
    if (saved === undefined || saved === null) return { kind: 'absent' };
    if (typeof saved !== 'number' || !Number.isSafeInteger(saved) || saved < 0) {
      return { kind: 'unknown' };
    }
    return { kind: 'known', value: saved };
  } catch {
    return { kind: 'unknown' };
  }
}

/** The sequence this instance last sent, or `null` when that is not knowable. */
export async function readReportSeq(): Promise<number | null> {
  const read = await readSeq();
  return read.kind === 'known' ? read.value : null;
}

/**
 * The next value to send: one past the recorded one, or 1 for an instance that
 * has never sent one.
 *
 * The write is verified by reading it back, exactly as `getInstallIdentity`
 * verifies its own record: `storage.local` has no compare-and-swap, so a value
 * this function cannot confirm is not a value it may stamp on a message. An
 * unconfirmed write returns `null` — "unknown" — instead of a number.
 */
export async function nextReportSeq(): Promise<number | null> {
  const storage = localStorageArea();
  if (!storage) return null;
  const read = await readSeq();
  // 🔴 A corrupt stored value is *left alone*. Overwriting it would destroy the
  //    only evidence that this writer's sequence is unusable, and the field
  //    would then look healthy again on the next read.
  if (read.kind === 'unknown') return null;
  const next = (read.kind === 'known' ? read.value : 0) + 1;
  try {
    await storage.set({ [REPORT_SEQ_KEY]: next });
    const confirmed = await storage.get({ [REPORT_SEQ_KEY]: null });
    return confirmed[REPORT_SEQ_KEY] === next ? next : null;
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
export async function resetReportSeq(): Promise<void> {
  const storage = localStorageArea();
  if (!storage) return;
  try {
    await storage.set({ [REPORT_SEQ_KEY]: null });
  } catch {
    // Deliberately swallowed: see above. The new identity is already persisted.
  }
}

/**
 * Stamp and send, one at a time (rule ② in the header).
 *
 * The chain is shared by every sequence-bearing message, and it must survive a
 * rejected link: a send that fails, or that times out, has to release the next
 * message rather than wedge it. `send` receives `null` when the sequence is
 * unknown, and the caller omits the field in that case.
 */
let seqChain: Promise<unknown> = Promise.resolve();

export function withReportSeq<T>(send: (seq: number | null) => Promise<T>): Promise<T> {
  const run = seqChain.then(async () => send(await nextReportSeq()));
  seqChain = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}
