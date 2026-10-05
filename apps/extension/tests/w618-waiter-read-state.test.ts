/**
 * W618 · What a waiter returns when its reader could not read.
 *
 * The suite's waiters poll until the record they were asked for appears. The
 * readers underneath them (`readStorage`, `readOutbox`) deliberately **throw**
 * rather than return an empty result, and each says why: "so a spec can never
 * read 'I could not ask' as 'there is none'". That is this project's first
 * invariant, and the throw is the whole mechanism.
 *
 * W618 added a retry around that throw, because a worker restarting mid-poll is
 * real and must not decide a test. The retry is right; what it must not do is
 * swallow the throw *forever*. A waiter that returns its empty seed value after
 * a poll in which every read threw has turned "I could not read storage" into
 * "storage was read and it was empty" — and the call site cannot tell those
 * apart, because both arrive as `{}` / `[]`. The caller would then report a
 * missing record for a run that never read anything, after burning the full
 * timeout, and under `retries: 0` that misleading failure is final.
 *
 * So the two halves are pinned separately here, because they are separate:
 *
 *  · a throw **after** a real reading is tolerated — the last reading stands,
 *    which is the restart tolerance W618 wanted;
 *  · a throw with **no** reading behind it propagates — the diagnostic that
 *    names the real cause is not discarded.
 *
 * Both are driven through the real readers and the real waiters, with only the
 * service worker faked, so what is pinned is the shipped poll shape rather than
 * a restatement of it.
 */

import { describe, it, expect } from 'vitest';
import { waitForOutbox, waitForStorage, waitForTickRecord, type Extension, type OutboxEntry } from '../e2e/harness';

/**
 * A worker whose `evaluate` runs `script`: either the reading it should return,
 * or the error it should throw. Each call advances the script by one, so a test
 * can say "this read throws, the next one succeeds" — the mid-poll restart —
 * with the reader's own code doing the rest.
 */
function worker(script: unknown[]) {
  let call = 0;
  return {
    evaluate: async () => {
      const step = script[Math.min(call, script.length - 1)];
      call += 1;
      if (step instanceof Error) throw step;
      return step;
    },
  };
}

/** An extension whose only worker is the one above; `[]` means "none running". */
function extension(workerOrNone: { evaluate: unknown } | null): Extension {
  return {
    context: { serviceWorkers: () => (workerOrNone ? [workerOrNone] : []) },
  } as unknown as Extension;
}

const OUTBOX_ROW: OutboxEntry = {
  sha256: 'a'.repeat(64),
  name: 'synthetic-session.jsonl',
  payload: '{"synthetic":true}',
  state: 'pending',
  enqueuedAt: 1,
} as OutboxEntry;

const NEVER = () => false;

describe('W618 · a waiter that could not read anything', () => {
  it('waitForStorage throws rather than reporting an empty storage', async () => {
    // No service worker at all: the extension never installed, or the worker
    // never came up. `readStorage` throws for exactly this reason.
    await expect(waitForStorage(extension(null), ['k'], NEVER, 60)).rejects.toThrow(
      /storage was not read/,
    );
  });

  it('waitForOutbox throws rather than reporting an empty outbox', async () => {
    // `[]` here would say "the outbox exists and is empty". The outbox was never
    // read, so that sentence is a lie a spec would then assert against.
    await expect(waitForOutbox(extension(null), NEVER, 60)).rejects.toThrow(
      /outbox was not read/,
    );
  });

  it('waitForTickRecord throws rather than reporting a snapshot with no record', async () => {
    await expect(waitForTickRecord(extension(null), { timeoutMs: 60 })).rejects.toThrow(
      /storage was not read/,
    );
  });

  it('returns the last real reading when the reader breaks after a good read', async () => {
    // The honest-return case: a read succeeded first, so there is a real reading
    // to stand on, and the last one is returned even though the reader then
    // broke for good. This must stay a return rather than becoming a throw.
    const rows = await waitForOutbox(
      extension(worker([[OUTBOX_ROW], new Error('outbox: read failed')])),
      () => false,
      60,
    );
    expect(rows).toEqual([OUTBOX_ROW]);
  });
});

describe('W618 · a waiter whose reader restarted mid-poll', () => {
  it('waitForStorage tolerates a throw before the first real reading', async () => {
    // The restart W618 was written for: the first read throws, the second
    // answers, and the wait returns the reading rather than failing.
    const state = { 'cs_test_key': { suspended: true } };
    const result = await waitForStorage(
      extension(worker([new Error('storage: no service worker is running'), state])),
      ['cs_test_key'],
      (reading) => Boolean((reading['cs_test_key'] as { suspended?: unknown } | undefined)?.suspended),
      500,
    );
    expect(result).toEqual(state);
  });

  it('waitForStorage keeps the last real reading when later reads throw', async () => {
    const state = { 'cs_test_key': { suspended: true } };
    const result = await waitForStorage(
      extension(worker([state, new Error('storage: read failed')])),
      ['cs_test_key'],
      NEVER,
      60,
    );
    // The reading that was taken, not an empty one invented for the timeout.
    expect(result).toEqual(state);
  });

  it('waitForOutbox keeps the last real reading when later reads throw', async () => {
    const result = await waitForOutbox(
      extension(worker([[OUTBOX_ROW], new Error('outbox: read failed')])),
      NEVER,
      60,
    );
    expect(result).toEqual([OUTBOX_ROW]);
  });
});