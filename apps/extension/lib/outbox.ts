/**
 * The outbox — a persistent, append-only queue of payloads that have not been
 * acknowledged yet.
 *
 * Normative text: `contracts/nativehost-protocol.md` §10 (extension-side
 * obligations) and §8 (the export file).
 *
 * Why it exists: the live leg must write a capture down *before* it attempts a
 * delivery, so a service worker killed between "the page produced bytes" and
 * "the host answered ack" cannot lose a conversation without a trace. The
 * outbox is the place that capture waits in.
 *
 * 🔴 Four rules, all of them load-bearing:
 *  1. **An entry is removed only on a matching `ack`** (see native-host.ts §1).
 *     Nothing else deletes: not a timeout, not a `nack`, not capacity pressure,
 *     not a retry budget.
 *  2. **The outbox never drops an item to make room.** When it is full it
 *     refuses the *new* capture and says so — loudly, to the caller.
 *  3. **A retryable failure keeps the entry pending** with an exponential
 *     backoff (1 minute, doubling, capped at 1 hour).
 *  4. **A non-retryable `nack` moves the entry to `rejected`** — kept, visible
 *     and included in the export, never retried. The host's own `retryable`
 *     flag is the authority (§6.3).
 *
 * Storage: IndexedDB, not `storage.local`. Payloads are up to a few MiB each
 * and `storage.local` is not built for that; IndexedDB is available to service
 * workers and to extension pages under the same origin, which is what lets the
 * popup export what the worker queued.
 */

import { deliver, isItemRejected, sha256Hex, type DeliverResult } from './native-host';
import { deliveryFingerprint } from './recapture';
import type { BackfillStore } from './backfill/store';

export const OUTBOX_DB_NAME = 'chat-stasher-outbox';
export const OUTBOX_DB_VERSION = 1;
export const OUTBOX_STORE = 'entries';
export const OUTBOX_META_STORE = 'meta';
/**
 * The running byte total's key in the meta store.
 * 🔴 Exported so a test can model "the outbox is nearly full" by writing this one
 *    counter — the alternative would be materialising a quarter gigabyte of
 *    payloads, which is slower, flakier and no truer. It is also the only key
 *    the module keeps in that store.
 */
export const OUTBOX_META_BYTES_KEY = 'bytes';

/**
 * Total capacity. 256 MiB is the ceiling the extension promises itself: big
 * enough that a long host outage (hundreds of captures) never hits it, small
 * enough that it cannot fill a small disk on its own. `unlimitedStorage` is in
 * the manifest, so the browser will not evict this behind our back.
 */
export const OUTBOX_CAPACITY_BYTES = 256 * 1024 * 1024;

/** Backoff for a retryable failure: 1 minute, doubling, capped at 1 hour. */
export const RETRY_BASE_MS = 60_000;
export const RETRY_MAX_MS = 3_600_000;

/** Where the last export happened. Written by the popup, read by the popup. */
export const LAST_EXPORT_KEY = 'cs_outbox_last_export_v1';

export type EntryState = 'pending' | 'rejected';

export interface OutboxEntry {
  /** Primary key: SHA-256 (lowercase hex) of the UTF-8 bytes of `payload`. */
  sha256: string;
  /** §6.2 `name` — the shard's `source_file` on the host side. */
  name: string;
  /** §6.2 `payload` — the bundle serialised with JSON.stringify. */
  payload: string;
  bytes: number;
  enqueuedAt: number;
  /** Failed delivery attempts so far. Never reset — it drives the backoff. */
  attempts: number;
  lastError: string | null;
  lastAttemptAt: number | null;
  state: EntryState;
  /** The `nack` kind that rejected this entry. Only on `state: 'rejected'`. */
  rejectKind?: string;
}

export interface OutboxSummary {
  pending: number;
  rejected: number;
  bytes: number;
  capacityBytes: number;
  /** True when nothing more can be accepted. Drives the alert badge. */
  full: boolean;
}

export interface OutboxOptions {
  /**
   * Override the capacity. The production call sites never pass it — the
   * default is the protocol's 256 MiB. It exists so the capacity rule can be
   * exercised without materialising a quarter gigabyte in a test.
   */
  capacityBytes?: number;
  /** Injectable clock. Defaults to `Date.now`. */
  now?: () => number;
}

export type EnqueueRefusal = 'outbox-unavailable' | 'outbox-full' | 'crypto-unavailable';

export interface EnqueueResult {
  accepted: boolean;
  /** Set when the identical payload was already queued (sha256 is the key). */
  duplicate?: boolean;
  sha256?: string;
  entry?: OutboxEntry;
  reason?: EnqueueRefusal;
  summary?: OutboxSummary;
}

// ---------------------------------------------------------------------------
// IndexedDB plumbing
// ---------------------------------------------------------------------------

function idbFactory(): IDBFactory | null {
  const g = globalThis as { indexedDB?: IDBFactory };
  return g.indexedDB ?? null;
}

let dbPromise: Promise<IDBDatabase | null> | null = null;

function openDb(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  const factory = idbFactory();
  if (!factory) {
    dbPromise = Promise.resolve(null);
    return dbPromise;
  }
  dbPromise = new Promise<IDBDatabase | null>((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      request = factory.open(OUTBOX_DB_NAME, OUTBOX_DB_VERSION);
    } catch {
      resolve(null);
      return;
    }
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(OUTBOX_STORE)) {
        const store = db.createObjectStore(OUTBOX_STORE, { keyPath: 'sha256' });
        store.createIndex('enqueuedAt', 'enqueuedAt');
        store.createIndex('state', 'state');
      }
      if (!db.objectStoreNames.contains(OUTBOX_META_STORE)) {
        db.createObjectStore(OUTBOX_META_STORE);
      }
    };
    request.onsuccess = () => resolve(request.result);
    // 🔴 A database that will not open is a named refusal for the caller, never
    // a silent "the queue was empty".
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
  return dbPromise;
}

/**
 * Does this context have an IndexedDB API at all?
 *
 * This is a different question from "can the outbox be read right now".
 * Without the API the outbox cannot exist, so nothing can ever have been
 * queued: "empty" is then a certainty, not an assumption. A failed read on a
 * context that *does* have the API is unknown, and callers must keep treating
 * that as unknown.
 */
export function outboxApiPresent(): boolean {
  return idbFactory() !== null;
}

/** Only for tests that model a service-worker restart inside one process. */
export function resetOutboxConnectionForTest(): void {
  dbPromise = null;
}

function requestToPromise<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error('IndexedDB request failed'));
  });
}

function txDone(tx: IDBTransaction): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? new Error('IndexedDB transaction failed'));
    tx.onabort = () => reject(tx.error ?? new Error('IndexedDB transaction aborted'));
  });
}

/**
 * 🔴 `null` means "could not be read", not "empty".
 * Every read interface in this module obeys that: when IndexedDB cannot be
 * opened it **never** returns an empty collection — "not one item is waiting" and
 * "I do not know whether any are" must be two different states in this project
 * (CLAUDE.md invariant 1: an unknown must never be recorded as empty).
 */
async function readAll(): Promise<OutboxEntry[] | null> {
  const db = await openDb();
  if (!db) return null;
  const tx = db.transaction(OUTBOX_STORE, 'readonly');
  const rows = await requestToPromise(tx.objectStore(OUTBOX_STORE).getAll() as IDBRequest<OutboxEntry[]>);
  return rows.sort(compareEntries);
}

/** FIFO by enqueue time, with the sha as a tie-break so the order is total. */
function compareEntries(a: OutboxEntry, b: OutboxEntry): number {
  if (a.enqueuedAt !== b.enqueuedAt) return a.enqueuedAt - b.enqueuedAt;
  return a.sha256 < b.sha256 ? -1 : (a.sha256 > b.sha256 ? 1 : 0);
}

function capacityOf(options: OutboxOptions): number {
  return options.capacityBytes ?? OUTBOX_CAPACITY_BYTES;
}

async function readBytes(db: IDBDatabase): Promise<number> {
  const tx = db.transaction(OUTBOX_META_STORE, 'readonly');
  const value = await requestToPromise(
    tx.objectStore(OUTBOX_META_STORE).get(OUTBOX_META_BYTES_KEY) as IDBRequest<number | undefined>,
  );
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : 0;
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/** All entries, or `null` when the outbox cannot be read at all. */
export async function listEntries(): Promise<OutboxEntry[] | null> {
  return await readAll();
}

/** The result of looking one entry up. Three states: found / genuinely absent / could not be read. */
export type EntryLookup =
  | { ok: true; entry: OutboxEntry | null }
  | { ok: false; reason: 'outbox-unavailable' };

/**
 * Look one entry up by content hash.
 * 🔴 `{ok:true, entry:null}` means "this payload is not in the outbox" — and the
 * only way that happens is a matching `ack` having deleted it. `{ok:false}`
 * means we cannot tell, and a caller must never read that as "delivered".
 */
export async function getEntry(sha256: string): Promise<EntryLookup> {
  const db = await openDb();
  if (!db) return { ok: false, reason: 'outbox-unavailable' };
  const tx = db.transaction(OUTBOX_STORE, 'readonly');
  const row = await requestToPromise(
    tx.objectStore(OUTBOX_STORE).get(sha256) as IDBRequest<OutboxEntry | undefined>,
  );
  return { ok: true, entry: row ?? null };
}

/** Entries that have not been delivered: pending ones and rejected ones. §8. */
export async function undeliveredEntries(): Promise<OutboxEntry[] | null> {
  const rows = await readAll();
  if (rows === null) return null;
  return rows.filter((e) => e.state === 'pending' || e.state === 'rejected');
}

/** `null` = the outbox could not be read; never a zeroed summary. */
export async function summary(options: OutboxOptions = {}): Promise<OutboxSummary | null> {
  const rows = await readAll();
  if (rows === null) return null;
  const capacityBytes = capacityOf(options);
  const bytes = rows.reduce((sum, e) => sum + (Number.isFinite(e.bytes) ? e.bytes : 0), 0);
  return {
    pending: rows.filter((e) => e.state === 'pending').length,
    rejected: rows.filter((e) => e.state === 'rejected').length,
    bytes,
    capacityBytes,
    full: bytes >= capacityBytes,
  };
}

/** When this entry may be attempted again. `null` = right now. */
export function dueAt(entry: OutboxEntry): number | null {
  if (entry.state !== 'pending') return null;
  if (entry.lastAttemptAt === null || entry.attempts <= 0) return null;
  return entry.lastAttemptAt + backoffMs(entry.attempts);
}

/** 1 min after the first failure, doubling, capped at 1 hour. */
export function backoffMs(attempts: number): number {
  if (attempts <= 0) return 0;
  // Cap the exponent before shifting so 2 ** n cannot overflow into Infinity.
  const exponent = Math.min(attempts - 1, 20);
  return Math.min(RETRY_BASE_MS * (2 ** exponent), RETRY_MAX_MS);
}

export function isDue(entry: OutboxEntry, now: number): boolean {
  const at = dueAt(entry);
  return at === null || now >= at;
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/**
 * Write-ahead one live capture.
 *
 * The sha256 of the payload is the primary key, so an identical payload already
 * in the queue is not queued twice — it is the *same* conversation by content
 * (the same key the host dedupes on, §7).
 */
export async function enqueue(
  name: string,
  payload: string,
  options: OutboxOptions = {},
): Promise<EnqueueResult> {
  const now = options.now ?? Date.now;
  const db = await openDb();
  if (!db) return { accepted: false, reason: 'outbox-unavailable' };

  const sha256 = await sha256Hex(payload);
  if (sha256 === null) return { accepted: false, reason: 'crypto-unavailable' };

  const bytes = new TextEncoder().encode(payload).byteLength;
  const capacityBytes = capacityOf(options);

  const tx = db.transaction([OUTBOX_STORE, OUTBOX_META_STORE], 'readwrite');
  const store = tx.objectStore(OUTBOX_STORE);
  const existing = await requestToPromise(store.get(sha256) as IDBRequest<OutboxEntry | undefined>);
  if (existing) {
    await txDone(tx);
    return { accepted: true, duplicate: true, sha256, entry: existing };
  }

  const meta = tx.objectStore(OUTBOX_META_STORE);
  const currentBytes = await requestToPromise(meta.get(OUTBOX_META_BYTES_KEY) as IDBRequest<number | undefined>);
  const used = typeof currentBytes === 'number' && currentBytes > 0 ? currentBytes : 0;
  if (used + bytes > capacityBytes) {
    // 🔴 Refuse the newcomer. Not one existing entry is touched (§10).
    const stateIndex = store.index('state');
    const [pendingCount, rejectedCount] = await Promise.all([
      requestToPromise(stateIndex.count('pending')),
      requestToPromise(stateIndex.count('rejected')),
    ]);
    // No abort: this transaction has only ever read, so committing does nothing.
    // (Calling abort() on a transaction that has already auto-committed throws
    // InvalidStateError — which would turn "refused" into a thrown error, exactly
    // the outcome this path must not have.)
    return {
      accepted: false,
      reason: 'outbox-full',
      sha256,
      // The counts describe what is actually queued, so the caller can show the
      // user what is at stake. `full` is the refusal itself, not a guess.
      summary: {
        pending: pendingCount,
        rejected: rejectedCount,
        bytes: used,
        capacityBytes,
        full: true,
      },
    };
  }

  const entry: OutboxEntry = {
    sha256,
    name,
    payload,
    bytes,
    enqueuedAt: now(),
    attempts: 0,
    lastError: null,
    lastAttemptAt: null,
    state: 'pending',
  };
  store.put(entry);
  meta.put(used + bytes, OUTBOX_META_BYTES_KEY);
  await txDone(tx);
  return { accepted: true, sha256, entry };
}

/** 🔴 The only deletion in this module. Callers must hold a matching `ack`. */
export async function markDelivered(sha256: string): Promise<void> {
  const db = await openDb();
  if (!db) return;
  const tx = db.transaction([OUTBOX_STORE, OUTBOX_META_STORE], 'readwrite');
  const store = tx.objectStore(OUTBOX_STORE);
  const existing = await requestToPromise(store.get(sha256) as IDBRequest<OutboxEntry | undefined>);
  if (existing) {
    store.delete(sha256);
    const meta = tx.objectStore(OUTBOX_META_STORE);
    const currentBytes = await requestToPromise(meta.get(OUTBOX_META_BYTES_KEY) as IDBRequest<number | undefined>);
    const used = typeof currentBytes === 'number' && currentBytes > 0 ? currentBytes : 0;
    meta.put(Math.max(0, used - existing.bytes), OUTBOX_META_BYTES_KEY);
  }
  await txDone(tx);
}

export interface FailureInput {
  reason: string;
  at: number;
  /** From the host's `nack` when there was one. */
  retryable: boolean;
  kind?: string;
}

/**
 * Record a failed delivery attempt.
 *
 * Retryable ⇒ stay pending, attempts + 1, exponential backoff.
 * Non-retryable ⇒ move to `rejected` (kept, listed, exported, never retried).
 * Either way the entry itself stays: this function never deletes.
 */
export async function recordFailure(sha256: string, input: FailureInput): Promise<OutboxEntry | null> {
  const db = await openDb();
  if (!db) return null;
  const tx = db.transaction(OUTBOX_STORE, 'readwrite');
  const store = tx.objectStore(OUTBOX_STORE);
  const existing = await requestToPromise(store.get(sha256) as IDBRequest<OutboxEntry | undefined>);
  if (!existing) {
    await txDone(tx);
    return null;
  }
  const next: OutboxEntry = {
    ...existing,
    attempts: existing.attempts + 1,
    lastError: input.kind ? `${input.reason}:${input.kind}` : input.reason,
    lastAttemptAt: input.at,
    state: input.retryable ? 'pending' : 'rejected',
  };
  if (!input.retryable) next.rejectKind = input.kind ?? input.reason;
  store.put(next);
  await txDone(tx);
  return next;
}

// ---------------------------------------------------------------------------
// Export file (§8)
// ---------------------------------------------------------------------------

/**
 * §8 file name: `chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl`.
 * Built from the ISO form so it is UTC by construction, never local time.
 */
export function exportFilename(at: number): string {
  const iso = new Date(at).toISOString(); // 2026-09-12T21:47:00.123Z
  const stamp = `${iso.slice(0, 4)}${iso.slice(5, 7)}${iso.slice(8, 10)}`
    + `T${iso.slice(11, 13)}${iso.slice(14, 16)}${iso.slice(17, 19)}Z`;
  return `chat-stasher-export-${stamp}.jsonl`;
}

export interface ExportFile {
  filename: string;
  /** One line per payload: the exact payload string plus "\n". */
  content: string;
  entries: number;
  bytes: number;
}

/**
 * The manual escape hatch. Each line is the exact `payload` string followed by
 * `\n` — byte-for-byte what `deliver` would have sent, which is what makes the
 * CLI's line hash equal the delivery hash.
 *
 * 🔴 Exporting does not remove anything: the entries stay queued, and the host
 * will eventually answer `duplicate` for them (§7).
 */
export function buildExportFile(entries: readonly OutboxEntry[], at: number): ExportFile {
  const ordered = [...entries].sort(compareEntries);
  let content = '';
  let bytes = 0;
  for (const entry of ordered) {
    content += entry.payload;
    content += '\n';
    bytes += entry.bytes + 1;
  }
  return { filename: exportFilename(at), content, entries: ordered.length, bytes };
}

export interface LastExport {
  at: number;
  entries: number;
  bytes: number;
  filename: string;
}

export async function loadLastExport(store: BackfillStore | null): Promise<LastExport | null> {
  if (!store) return null;
  const raw = await store.load(LAST_EXPORT_KEY);
  if (!raw || typeof raw !== 'object') return null;
  const rec = raw as Partial<LastExport>;
  if (typeof rec.at !== 'number' || typeof rec.entries !== 'number') return null;
  return {
    at: rec.at,
    entries: rec.entries,
    bytes: typeof rec.bytes === 'number' ? rec.bytes : 0,
    filename: typeof rec.filename === 'string' ? rec.filename : '',
  };
}

export async function recordExport(store: BackfillStore | null, rec: LastExport): Promise<void> {
  if (!store) return;
  await store.save(LAST_EXPORT_KEY, rec);
}

// ---------------------------------------------------------------------------
// Draining
// ---------------------------------------------------------------------------

export type DeliveryFn = (
  name: string,
  payload: string,
  fingerprint: string | null,
) => Promise<DeliverResult>;

export interface DrainOptions {
  /** Test seam. Production always uses the real `deliver` from native-host. */
  deliver?: DeliveryFn;
  now?: () => number;
  /** Stop after this many attempts in one run. */
  maxPerRun?: number;
}

export type DrainStop =
  /** Nothing was due. */
  | 'idle'
  /** Every due entry was attempted and none is left pending. */
  | 'drained'
  /** A retryable failure — the host looks unavailable. Stopped early on purpose. */
  | 'host-unavailable'
  /** `maxPerRun` reached; more entries remain. */
  | 'batch'
  /** IndexedDB is not available in this context. */
  | 'outbox-unavailable';

export interface DrainReport {
  attempted: number;
  delivered: number;
  rejected: number;
  /** Entries skipped because their backoff had not elapsed. */
  waiting: number;
  stoppedBy: DrainStop;
  /** The named failure that stopped the run, when one did. */
  lastReason?: string;
  lastKind?: string;
  lastDetail?: string;
}

let inFlight: Promise<DrainReport> | null = null;

/**
 * Send what is due, one entry at a time, waiting for each result before the
 * next (§10: serial).
 *
 * 🔴 One drain at a time. Concurrent callers (a capture and an alarm firing
 * together) share the run already in progress instead of starting a second one.
 *
 * 🔴 A retryable failure ends the run immediately. If the host is missing,
 * every remaining entry would burn its own 60-second timeout to learn the same
 * fact; stopping keeps the whole unit of work bounded and matches the backfill
 * leg's behaviour.
 */
export function drainOutbox(options: DrainOptions = {}): Promise<DrainReport> {
  if (inFlight) return inFlight;
  const run = runDrain(options).finally(() => {
    if (inFlight === run) inFlight = null;
  });
  inFlight = run;
  return run;
}

async function runDrain(options: DrainOptions): Promise<DrainReport> {
  // 🔴 W91b · **This leg is deliberately not channel-filtered.** An entry here is
  //    user data a build already captured, not a platform this build serves: a
  //    dev build that captured Kimi while the native host was down leaves a
  //    `kimi-*.json` bundle queued, and a stable build that later becomes active
  //    must still deliver it. Dropping it because the platform is experimental in
  //    this channel would lose a conversation the user already has on disk.
  //    RELEASING.md states the same decision under "Release channel".
  const now = options.now ?? Date.now;
  const deliverFn: DeliveryFn = options.deliver ?? deliver;

  const report: DrainReport = {
    attempted: 0,
    delivered: 0,
    rejected: 0,
    waiting: 0,
    stoppedBy: 'idle',
  };

  const all = await readAll();
  if (all === null) {
    report.stoppedBy = 'outbox-unavailable';
    return report;
  }
  const due = all
    .filter((e) => e.state === 'pending')
    .filter((e) => {
      if (isDue(e, now())) return true;
      report.waiting += 1;
      return false;
    });

  if (due.length === 0) {
    report.stoppedBy = report.waiting > 0 ? 'idle' : 'drained';
    return report;
  }

  for (const entry of due) {
    if (options.maxPerRun !== undefined && report.attempted >= options.maxPerRun) {
      report.stoppedBy = 'batch';
      return report;
    }
    // 🔴 W50c · The §6.6 fingerprint travels with the bytes, derived here from the
    //    payload this attempt is about to send (`lib/recapture.ts`
    //    `deliveryFingerprint`, which is where that decision is argued). It is
    //    metadata the host records on the shard, never a delivery precondition: a
    //    payload it cannot be derived from is delivered without one, exactly as
    //    before this field existed.
    const result = await deliverFn(entry.name, entry.payload, await deliveryFingerprint(entry.payload));
    report.attempted += 1;

    if (result.delivered) {
      // 🔴 The only place an entry is ever removed.
      await markDelivered(entry.sha256);
      report.delivered += 1;
      continue;
    }

    // §6.3: scope, not `retryable` alone, decides the item's fate. A host-scope
    // nack (e.g. `config`, retryable:false) keeps the item pending.
    const itemRejected = isItemRejected(result);
    await recordFailure(entry.sha256, {
      reason: result.reason,
      kind: result.kind,
      at: now(),
      retryable: !itemRejected,
    });
    report.lastReason = result.reason;
    report.lastKind = result.kind;
    report.lastDetail = result.detail;

    if (itemRejected) {
      report.rejected += 1;
      continue; // this entry is at fault, not the host — try the next one
    }
    report.stoppedBy = 'host-unavailable';
    return report;
  }

  report.stoppedBy = 'drained';
  return report;
}
