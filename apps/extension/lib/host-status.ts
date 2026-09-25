/**
 * What we know about the native host, written down.
 *
 * Two separate facts, deliberately not merged:
 *
 *  1. **The last `hello` result** — the answer to "is the host there, and where
 *     does it write?" (§6.1), with the time it was asked. The popup renders
 *     this instead of probing on its own, so the popup never has to guess and
 *     never blocks.
 *  2. **The backfill pause** — "we tried to deliver a backfill item and could
 *     not". §10 requires that an undelivered backfill item keeps its debt open
 *     and that backfill pauses *with a visible reason* until a `hello` succeeds
 *     again. This record is that reason, and it is what the next heartbeat
 *     consults before doing anything else.
 *
 * Both live in `storage.local` (`cs_*`, the same family the debt set uses) so
 * they survive a service-worker recycle — which is the only reason a pause can
 * be said to be visible at all.
 */

import { hello, type HelloResult, type NackKind } from './native-host';
import type { BackfillStore } from './backfill/store';

export const HOST_STATUS_KEY = 'cs_native_host_status_v1';
export const HOST_PAUSE_KEY = 'cs_native_host_pause_v1';

/** The one pause reason this build produces. Named, never a bare boolean. */
export const HOST_UNAVAILABLE = 'host-unavailable';

export interface HostStatusRecord {
  /** When the check ran (ms). */
  at: number;
  ok: boolean;
  /** Named reason when `ok` is false — see native-host.ts. */
  reason?: string;
  kind?: NackKind | string;
  detail?: string;
  machine?: string;
  stage?: string;
  hostVersion?: string;
  /**
   * The most recent stage path that *was* successfully reported, kept across
   * failures so the popup can print a fix command with the real path in it.
   * Labelled "last known" wherever it is shown — it is not a current fact.
   */
  lastKnownStage?: string;
}

export interface HostPauseRecord {
  reason: string;
  at: number;
  detail?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

export async function loadHostStatus(store: BackfillStore | null): Promise<HostStatusRecord | null> {
  if (!store) return null;
  const raw = await store.load(HOST_STATUS_KEY);
  if (!isRecord(raw) || typeof raw.at !== 'number' || typeof raw.ok !== 'boolean') return null;
  return raw as unknown as HostStatusRecord;
}

export async function loadHostPause(store: BackfillStore | null): Promise<HostPauseRecord | null> {
  if (!store) return null;
  const raw = await store.load(HOST_PAUSE_KEY);
  if (!isRecord(raw) || typeof raw.at !== 'number' || typeof raw.reason !== 'string') return null;
  return { reason: raw.reason, at: raw.at, detail: typeof raw.detail === 'string' ? raw.detail : undefined };
}

/**
 * Run `hello` once and write the result down. Returns the record either way —
 * a failed check is a fact we keep, not an error we swallow.
 */
export async function checkHost(
  store: BackfillStore | null,
  options: { timeoutMs?: number; now?: number } = {},
): Promise<HostStatusRecord> {
  const at = options.now ?? Date.now();
  const result = await hello(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs });
  const previous = await loadHostStatus(store);
  const record = toRecord(result, at, previous);
  await saveHostStatus(store, record);
  return record;
}

export function toRecord(
  result: HelloResult,
  at: number,
  previous: HostStatusRecord | null,
): HostStatusRecord {
  const lastKnownStage = previous?.stage ?? previous?.lastKnownStage;
  if (result.ok) {
    return {
      at,
      ok: true,
      machine: result.machine,
      stage: result.stage,
      hostVersion: result.hostVersion,
      lastKnownStage: result.stage,
    };
  }
  return {
    at,
    ok: false,
    reason: result.reason,
    kind: result.kind,
    detail: result.detail,
    lastKnownStage,
  };
}

async function saveHostStatus(store: BackfillStore | null, record: HostStatusRecord): Promise<void> {
  if (!store) return;
  try {
    await store.save(HOST_STATUS_KEY, record);
  } catch (err) {
    console.warn('[chat-stasher] host status write failed', (err as Error).message);
  }
}

export async function setHostPause(
  store: BackfillStore | null,
  record: HostPauseRecord,
): Promise<void> {
  if (!store) return;
  try {
    await store.save(HOST_PAUSE_KEY, record);
  } catch (err) {
    console.warn('[chat-stasher] host pause write failed', (err as Error).message);
  }
}

export async function clearHostPause(store: BackfillStore | null): Promise<void> {
  if (!store) return;
  try {
    await store.save(HOST_PAUSE_KEY, null);
  } catch (err) {
    console.warn('[chat-stasher] host pause clear failed', (err as Error).message);
  }
}

/**
 * The heartbeat's recovery step (§10): ask the host `hello`, and only on `ok`
 * clear the pause. A failed `hello` leaves the pause exactly as it was —
 * "the host did not answer" never resumes anything.
 */
export async function resumeBackfill(
  store: BackfillStore | null,
): Promise<{ resumed: boolean; status: HostStatusRecord }> {
  const status = await checkHost(store);
  if (!status.ok) return { resumed: false, status };
  await clearHostPause(store);
  return { resumed: true, status };
}

/**
 * 🔴 W50b · **`probeDestination` lived here and is gone, deliberately.**
 *
 * It asked the host where it writes so the recapture guard could compare that with
 * the destination a remembered delivery named. W50c removed the comparison and the
 * remembered destination with it: the guard now asks the host one question — §6.6
 * `has`, "do you already hold this content?" — and the host answers it **from the
 * stage it is writing to**. A destination the extension holds is therefore a second
 * copy of an answer the archive gives directly, and the weaker copy: it is a string
 * captured at ack time, and the event it cannot see (the archive replaced at the same
 * path) is exactly the one the guard exists for.
 *
 * Nothing here replaces it. `checkHost` above is unaffected and still serves the
 * popup; the guard no longer reads the host's identity at all.
 */
