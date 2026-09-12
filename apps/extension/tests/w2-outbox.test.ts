/**
 * W2 · The outbox: write-ahead, delete only on ack, backoff, capacity, export.
 *
 * Spec §10's extension-side obligations are pinned here one by one:
 *   · a live capture goes into the outbox before any delivery attempt (write-ahead), and is
 *     **deleted only on a matching ack**;
 *   · the outbox **never** drops an entry to make room — when full it refuses the new one and
 *     says so;
 *   · a non-retryable nack ⇒ into `rejected` (kept, visible, included in the export), with no
 *     further automatic retry;
 *   · draining is serial: exactly one runs at a time.
 *
 * "The SW is killed" is simulated by **creating a new module instance reading the same
 * IndexedDB**: re-importing after `vi.resetModules()` is the same thing as an MV3 worker being
 * reclaimed and woken again.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import type { DeliverResult } from '../lib/native-host';

const DELIVERED: DeliverResult = {
  delivered: true, status: 'stored', shard: '0001-x.json', requestId: 'r', sha256: 's',
};
const RETRYABLE: DeliverResult = {
  delivered: false, reason: 'send-failed', retryable: true, requestId: 'r', sha256: 's',
};
const REJECTED: DeliverResult = {
  delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
  detail: 'payload is not an inbox bundle', requestId: 'r', sha256: 's',
};

/** Every case starts from a brand-new, empty IndexedDB. */
function freshIdb(): void {
  (globalThis as any).indexedDB = new IDBFactory();
}

async function outbox() {
  return await import('../lib/outbox');
}

const NAME_A = 'chatgpt-aaaaaaaa-1111-2222-3333-444444444444.json';
const PAYLOAD_A = '{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444"}';
const NAME_B = 'chatgpt-bbbbbbbb-1111-2222-3333-444444444444.json';
const PAYLOAD_B = '{"sessionId":"bbbbbbbb-1111-2222-3333-444444444444"}';

beforeEach(() => {
  vi.resetModules();
  freshIdb();
});

// ===========================================================================
// 1 · write-ahead and "delete only on ack"
// ===========================================================================
describe('W2-OUTBOX · write-ahead and delete-only-on-ack', () => {
  it('after enqueueing the entry is on disk: the primary key is the sha256 of the payload bytes', async () => {
    const ob = await outbox();
    const res = await ob.enqueue(NAME_A, PAYLOAD_A);
    expect(res.accepted).toBe(true);
    expect(res.duplicate).toBeUndefined();

    const entries = await ob.listEntries();
    expect(entries).toHaveLength(1);
    expect(entries![0]).toMatchObject({
      sha256: res.sha256,
      name: NAME_A,
      payload: PAYLOAD_A,
      bytes: PAYLOAD_A.length,
      attempts: 0,
      lastError: null,
      lastAttemptAt: null,
      state: 'pending',
    });
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(PAYLOAD_A));
    const expected = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
    expect(res.sha256).toBe(expected);
  });

  it('🔴 a matching ack ⇒ the entry is deleted and the outbox is empty', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({ deliver: async () => DELIVERED });

    expect(report).toMatchObject({ attempted: 1, delivered: 1, rejected: 0, stoppedBy: 'drained' });
    expect(await ob.listEntries()).toEqual([]);
    expect(await ob.summary()).toMatchObject({ pending: 0, rejected: 0, bytes: 0 });
  });

  it('🔴 a timeout / lastError (retryable) ⇒ the entry is **still there**, with only attempts and lastError added', async () => {
    const ob = await outbox();
    const { sha256 } = await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({ deliver: async () => RETRYABLE, now: () => 1_000 });

    expect(report).toMatchObject({ attempted: 1, delivered: 0, rejected: 0, stoppedBy: 'host-unavailable' });
    const entry = (await ob.getEntry(sha256!)).ok ? (await ob.getEntry(sha256!)) as any : null;
    expect(entry.entry).toMatchObject({
      state: 'pending',
      attempts: 1,
      lastError: 'send-failed',
      lastAttemptAt: 1_000,
    });
    expect(await ob.listEntries()).toHaveLength(1);   // not one byte was lost
  });

  it('🔴 a non-conforming response (malformed-response, retryable) ⇒ same as above, the entry is still there', async () => {
    const ob = await outbox();
    const { sha256 } = await ob.enqueue(NAME_A, PAYLOAD_A);
    await ob.drainOutbox({
      deliver: async () => ({
        delivered: false, reason: 'malformed-response', detail: 'ack sha256 does not match',
        retryable: true, requestId: 'r', sha256: 's',
      }),
      now: () => 5_000,
    });
    const lookup = await ob.getEntry(sha256!);
    expect(lookup).toMatchObject({ ok: true, entry: { state: 'pending', attempts: 1 } });
  });
});

// ===========================================================================
// 2 · deduplication (sha256 is the primary key)
// ===========================================================================
describe('W2-OUTBOX · deduplication by content', () => {
  it('the same payload enqueued twice ⇒ only one entry, the second reports duplicate', async () => {
    const ob = await outbox();
    const first = await ob.enqueue(NAME_A, PAYLOAD_A);
    const second = await ob.enqueue(NAME_A, PAYLOAD_A);
    expect(second).toMatchObject({ accepted: true, duplicate: true, sha256: first.sha256 });
    expect(await ob.listEntries()).toHaveLength(1);
    expect((await ob.summary())!.bytes).toBe(PAYLOAD_A.length);
  });

  it('different content but the same name ⇒ two entries (content addressing, not dedup by name)', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A);
    await ob.enqueue(NAME_A, `${PAYLOAD_A} `);
    expect(await ob.listEntries()).toHaveLength(2);
  });
});

// ===========================================================================
// 3 · capacity
// ===========================================================================
describe('W2-OUTBOX · when full, refuse the new one and never touch the old', () => {
  it('🔴 full ⇒ the new entry is refused with a visible reason; the existing entries are all there, not one byte changed', async () => {
    const ob = await outbox();
    const capacityBytes = PAYLOAD_A.length + PAYLOAD_B.length;
    await ob.enqueue(NAME_A, PAYLOAD_A, { capacityBytes });
    await ob.enqueue(NAME_B, PAYLOAD_B, { capacityBytes });
    const before = await ob.listEntries();
    expect(before).toHaveLength(2);
    expect((await ob.summary({ capacityBytes }))).toMatchObject({
      pending: 2, bytes: capacityBytes, capacityBytes, full: true,
    });

    const third = await ob.enqueue('chatgpt-c.json', '{"sessionId":"cccccccc-1111-2222-3333-444444444444"}', {
      capacityBytes,
    });
    expect(third.accepted).toBe(false);
    expect(third.reason).toBe('outbox-full');
    // Even the refusal has to say "how much is actually piled up" — the number comes from real stock, not a guess.
    expect(third.summary).toMatchObject({ pending: 2, bytes: capacityBytes, full: true });

    const after = await ob.listEntries();
    expect(after).toEqual(before);          // 🔴 all there, not one byte changed
    expect((await ob.summary({ capacityBytes }))!.full).toBe(true);
  });

  it('below capacity it still accepts (this only proves the case above is not red for some other reason)', async () => {
    const ob = await outbox();
    const res = await ob.enqueue(NAME_A, PAYLOAD_A, { capacityBytes: 10_000 });
    expect(res.accepted).toBe(true);
  });
});

// ===========================================================================
// 4 · backoff
// ===========================================================================
describe('W2-OUTBOX · exponential backoff after a retryable failure', () => {
  it('🔴 after a failure it does **not** resend immediately; only once the backoff window has passed', async () => {
    const ob = await outbox();
    const attempts: number[] = [];
    let now = 1_000_000;
    const deliver = async (): Promise<DeliverResult> => {
      attempts.push(now);
      return RETRYABLE;
    };

    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => now });
    await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(1);

    // The backoff has not elapsed ⇒ not one send is allowed (this is also the "do not hammer the host" rule).
    now += ob.RETRY_BASE_MS - 1;
    const waiting = await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(1);
    expect(waiting).toMatchObject({ attempted: 0, waiting: 1 });

    // The window has passed ⇒ retry once.
    now += 1;
    await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(2);
    expect(attempts[1]! - attempts[0]!).toBe(ob.RETRY_BASE_MS);

    // A second failure ⇒ the backoff doubles: another 60 seconds is not enough; a full 120 are needed.
    const entries = await ob.listEntries();
    expect(entries![0]!.attempts).toBe(2);
    expect(ob.backoffMs(2)).toBe(2 * ob.RETRY_BASE_MS);
    now += ob.RETRY_BASE_MS;                 // 60 seconds since the last attempt
    expect((await ob.drainOutbox({ deliver, now: () => now })).attempted).toBe(0);
    now += ob.RETRY_BASE_MS;                 // 120 seconds since the last attempt
    await ob.drainOutbox({ deliver, now: () => now });
    expect(attempts).toHaveLength(3);
    expect(attempts[2]! - attempts[1]!).toBe(2 * ob.RETRY_BASE_MS);
  });

  it('the backoff is capped (1 hour) and does not double without bound', async () => {
    const ob = await outbox();
    console.log('[W2-OUTBOX] backoff sequence (ms):',
      [1, 2, 3, 4, 5, 10, 50].map((n) => ob.backoffMs(n)));
    expect(ob.backoffMs(1)).toBe(60_000);
    expect(ob.backoffMs(2)).toBe(120_000);
    expect(ob.backoffMs(3)).toBe(240_000);
    expect(ob.backoffMs(50)).toBe(3_600_000);
    expect(ob.backoffMs(0)).toBe(0);
  });
});

// ===========================================================================
// 5 · non-retryable ⇒ rejected, but kept, and present in the export
// ===========================================================================
describe('W2-OUTBOX · a non-retryable nack ⇒ rejected', () => {
  it('🔴 the entry is kept, marked rejected, carries the kind, and is **no longer** retried automatically', async () => {
    const ob = await outbox();
    const { sha256 } = await ob.enqueue(NAME_A, PAYLOAD_A);
    const report = await ob.drainOutbox({ deliver: async () => REJECTED, now: () => 9_000 });

    expect(report).toMatchObject({ attempted: 1, delivered: 0, rejected: 1, stoppedBy: 'drained' });
    const lookup = await ob.getEntry(sha256!);
    expect(lookup).toMatchObject({
      ok: true,
      entry: {
        state: 'rejected',
        rejectKind: 'invalid-bundle',
        lastError: 'nack:invalid-bundle',
        attempts: 1,
      },
    });

    // No amount of draining will touch it again.
    let calls = 0;
    await ob.drainOutbox({ deliver: async () => { calls += 1; return DELIVERED; }, now: () => 10_000_000 });
    expect(calls).toBe(0);

    // But it is still undelivered, so the export contains it.
    const undelivered = await ob.undeliveredEntries();
    expect(undelivered).toHaveLength(1);
    expect(undelivered![0]!.payload).toBe(PAYLOAD_A);
  });

  it('one item being judged dead does not affect the next: the same drain carries on with the rest', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    const seen: string[] = [];
    const report = await ob.drainOutbox({
      deliver: async (name) => {
        seen.push(name);
        return name === NAME_A ? REJECTED : DELIVERED;
      },
    });
    expect(seen).toEqual([NAME_A, NAME_B]);
    expect(report).toMatchObject({ attempted: 2, delivered: 1, rejected: 1 });
    const left = await ob.listEntries();
    expect(left!.map((e) => e.name)).toEqual([NAME_A]);
    expect(left![0]!.state).toBe('rejected');
  });
});

// ===========================================================================
// 6 · write-ahead really survives the SW being killed
// ===========================================================================
describe('W2-OUTBOX · the SW is killed before delivery', () => {
  it('🔴 in a new module instance (same database) the entry is still there, and the next drain sends it', async () => {
    const first = await outbox();
    const { sha256 } = await first.enqueue(NAME_A, PAYLOAD_A);
    expect(sha256).toBeTruthy();
    // 🔴 There is **no** drain here — simulating "written, then reclaimed before it went out".

    // The SW is reclaimed and wakes again: all module memory is gone, IndexedDB remains.
    vi.resetModules();
    const second = await outbox();
    const entries = await second.listEntries();
    expect(entries).toHaveLength(1);
    expect(entries![0]!.payload).toBe(PAYLOAD_A);

    const sent: string[] = [];
    const report = await second.drainOutbox({
      deliver: async (name, payload) => { sent.push(payload); return DELIVERED; },
    });
    expect(report.delivered).toBe(1);
    expect(sent).toEqual([PAYLOAD_A]);
    expect(await second.listEntries()).toEqual([]);   // deleted only after the ack
  });

  it('🔴 killed half way through a drain (the first was acked) ⇒ only that one disappears, the rest remain', async () => {
    const first = await outbox();
    // The enqueue times are written apart so the FIFO order is deterministic (within one millisecond it falls back to sorting by sha).
    await first.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await first.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    const order: string[] = [];
    await first.drainOutbox({
      deliver: async (name) => {
        order.push(name);
        // A succeeds (its entry is deleted), B finds the host absent (its entry stays and the round stops there).
        return name === NAME_A ? DELIVERED : RETRYABLE;
      },
      now: () => 3_000,
    });
    expect(order).toEqual([NAME_A, NAME_B]);

    vi.resetModules();
    const second = await outbox();
    const left = await second.listEntries();
    expect(left!.map((e) => e.name)).toEqual([NAME_B]);
    expect(left![0]!.state).toBe('pending');
  });
});

// ===========================================================================
// 7 · serial and mutually exclusive
// ===========================================================================
describe('W2-OUTBOX · draining is serial and mutually exclusive', () => {
  it('🔴 two concurrent triggers run only one drain and send nothing twice', async () => {
    const ob = await outbox();
    // The enqueue times are written apart so the FIFO order is deterministic (within one millisecond it falls back to sorting by sha).
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });

    const calls: string[] = [];
    let inFlightNow = 0;
    let maxConcurrent = 0;
    const deliver = async (name: string): Promise<DeliverResult> => {
      calls.push(name);
      inFlightNow += 1;
      maxConcurrent = Math.max(maxConcurrent, inFlightNow);
      await new Promise((resolve) => setTimeout(resolve, 1));
      inFlightNow -= 1;
      return DELIVERED;
    };

    // Triggered twice in the same beat (the model of one capture plus one alarm).
    const [a, b] = await Promise.all([
      ob.drainOutbox({ deliver }),
      ob.drainOutbox({ deliver }),
    ]);

    expect(calls).toEqual([NAME_A, NAME_B]);   // each sent exactly once
    expect(maxConcurrent).toBe(1);             // never two in flight at once
    expect(a).toBe(b);                         // the second trigger shares the same drain's result
    expect(a).toMatchObject({ attempted: 2, delivered: 2 });
    expect(await ob.listEntries()).toEqual([]);
  });

  it('on reaching maxPerRun it stops, leaving the rest for the next round (still pending)', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    const report = await ob.drainOutbox({ deliver: async () => DELIVERED, maxPerRun: 1 });
    expect(report).toMatchObject({ attempted: 1, delivered: 1, stoppedBy: 'batch' });
    expect((await ob.listEntries())!.map((e) => e.name)).toEqual([NAME_B]);
  });

  it('FIFO: whatever was enqueued first is sent first', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    const order: string[] = [];
    await ob.drainOutbox({ deliver: async (name) => { order.push(name); return DELIVERED; } });
    expect(order).toEqual([NAME_A, NAME_B]);
  });
});

// ===========================================================================
// 8 · unreadable ≠ empty
// ===========================================================================
describe('W2-OUTBOX · when IndexedDB cannot be read it must say "I do not know"', () => {
  beforeEach(() => {
    // There is no IndexedDB in this context.
    delete (globalThis as any).indexedDB;
  });

  it('🔴 summary() returns null (not 0) and listEntries() returns null (not [])', async () => {
    const ob = await outbox();
    expect(await ob.summary()).toBeNull();
    expect(await ob.listEntries()).toBeNull();
    expect(await ob.undeliveredEntries()).toBeNull();
  });

  it('🔴 the enqueue is refused by name (never a silent "stored")', async () => {
    const ob = await outbox();
    const res = await ob.enqueue(NAME_A, PAYLOAD_A);
    expect(res).toMatchObject({ accepted: false, reason: 'outbox-unavailable' });
  });

  it('🔴 getEntry says "could not read it" and never passes itself off as "that entry is not there"', async () => {
    const ob = await outbox();
    expect(await ob.getEntry('whatever')).toEqual({ ok: false, reason: 'outbox-unavailable' });
  });

  it('the drain honestly reports outbox-unavailable, and deliver was never called once', async () => {
    const ob = await outbox();
    let calls = 0;
    const report = await ob.drainOutbox({ deliver: async () => { calls += 1; return DELIVERED; } });
    expect(report.stoppedBy).toBe('outbox-unavailable');
    expect(calls).toBe(0);
  });
});

// ===========================================================================
// 9 · export (§8)
// ===========================================================================
describe('W2-OUTBOX · the export file', () => {
  it('🔴 the content equals each payload line by line (byte level), one \\n per line, in enqueue order', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    const entries = (await ob.listEntries())!;

    const at = Date.parse('2026-09-12T21:47:03.123Z');
    const file = ob.buildExportFile(entries, at);

    expect(file.content).toBe(`${PAYLOAD_A}\n${PAYLOAD_B}\n`);
    // Byte level: the file bytes = each payload's UTF-8 bytes plus one \n per line.
    const bytes = new TextEncoder().encode(file.content).byteLength;
    expect(file.bytes).toBe(bytes);
    expect(bytes).toBe(
      new TextEncoder().encode(PAYLOAD_A).byteLength + 1
      + new TextEncoder().encode(PAYLOAD_B).byteLength + 1,
    );
    // Splitting back line by line must equal the payloads character for character (no escaping, no quoting, no truncation).
    const lines = file.content.slice(0, -1).split('\n');
    expect(lines).toEqual([PAYLOAD_A, PAYLOAD_B]);
    expect(file.entries).toBe(2);
  });

  it('🔴 the file name is strictly chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl', async () => {
    const ob = await outbox();
    expect(ob.exportFilename(Date.parse('2026-09-12T21:47:03.123Z')))
      .toBe('chat-stasher-export-20260912T214703Z.jsonl');
    // Midnight and single-digit months/days are zero-padded too, and it is UTC rather than local time.
    expect(ob.exportFilename(Date.parse('2026-01-02T03:04:05.000Z')))
      .toBe('chat-stasher-export-20260102T030405Z.jsonl');
  });

  it('rejected entries are in the export too (§10: they are kept and exported)', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A, { now: () => 1_000 });
    await ob.enqueue(NAME_B, PAYLOAD_B, { now: () => 2_000 });
    await ob.drainOutbox({
      deliver: async (name) => (name === NAME_A ? REJECTED : RETRYABLE),
    });

    const file = ob.buildExportFile((await ob.undeliveredEntries())!, Date.now());
    expect(file.content).toBe(`${PAYLOAD_A}\n${PAYLOAD_B}\n`);
    expect(file.entries).toBe(2);
  });

  it('exporting **deletes nothing** (the host will confirm them as duplicates once it is back)', async () => {
    const ob = await outbox();
    await ob.enqueue(NAME_A, PAYLOAD_A);
    ob.buildExportFile((await ob.undeliveredEntries())!, Date.now());
    expect(await ob.listEntries()).toHaveLength(1);
  });

  it('an empty outbox ⇒ empty content, 0 entries (the exporter decides whether to produce a file)', async () => {
    const ob = await outbox();
    const file = ob.buildExportFile([], Date.now());
    expect(file).toMatchObject({ content: '', entries: 0, bytes: 0 });
  });

  it('the last-export record can be written and read back (it is what the popup shows)', async () => {
    const ob = await outbox();
    const { memoryStore } = await import('../lib/backfill/store');
    const store = memoryStore();
    expect(await ob.loadLastExport(store)).toBeNull();      // never exported ⇒ null, do not invent a row
    await ob.recordExport(store, {
      at: 1_700_000_000_000, entries: 3, bytes: 300, filename: 'chat-stasher-export-x.jsonl',
    });
    expect(await ob.loadLastExport(store)).toMatchObject({ entries: 3, bytes: 300 });
  });
});
