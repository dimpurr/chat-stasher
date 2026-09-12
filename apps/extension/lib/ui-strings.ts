/**
 * Every NEW piece of user-visible text lives here, in English, so the popup can
 * be localised later by swapping one module instead of hunting strings across
 * the render layer.
 *
 * 🔴 Scope rule: this file holds the strings introduced with the "one question,
 *    one answer, ack-only" transport — the channel status, the outbox, the
 *    export button and the host-availability pause. The older Chinese strings
 *    (status / running / missing / progress / coverage / failures) are NOT
 *    moved here; they belong to a separate i18n task. Do not "tidy" them into
 *    this file: that would change what the existing tests assert.
 *
 * Nothing here computes state. Each builder takes the facts it was handed and
 * states them; a missing fact becomes a missing fact in the sentence, never a
 * plausible default.
 */

import type { NackKind } from './native-host';

/** Placeholder used when we have never learned a stage path. */
export const STAGE_PLACEHOLDER = '<path-to-your-stage-dir>';

/**
 * The one command that fixes both "host not installed" and "host has no stage".
 * §6.3 lists `config` with the instruction that its `detail` names the fix.
 */
export function fixCommand(stage: string | null | undefined): string {
  return `chat-stasher install-native-host --stage ${stage && stage.length > 0 ? stage : STAGE_PLACEHOLDER}`;
}

/** Milliseconds since epoch → `YYYY-MM-DD HH:MM:SS UTC`, or an honest gap. */
export function stamp(at: number | null | undefined): string {
  if (typeof at !== 'number' || !Number.isFinite(at)) return 'unknown time';
  return `${new Date(at).toISOString().replace('T', ' ').slice(0, 19)} UTC`;
}

/** Bytes → a short human quantity. Used for outbox capacity, never for rates. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return 'unknown size';
  if (bytes < 1024) return `${bytes} B`;
  const kib = bytes / 1024;
  if (kib < 1024) return `${kib.toFixed(1)} KiB`;
  const mib = kib / 1024;
  if (mib < 1024) return `${mib.toFixed(1)} MiB`;
  return `${(mib / 1024).toFixed(2)} GiB`;
}

// ---------------------------------------------------------------------------
// Delivery channel
// ---------------------------------------------------------------------------

export interface ChannelFacts {
  at: number;
  ok: boolean;
  reason?: string;
  kind?: NackKind | string;
  detail?: string;
  machine?: string;
  stage?: string;
  hostVersion?: string;
  lastKnownStage?: string;
}

export const CHANNEL_NO_CHECK =
  'Delivery channel: no host check has run yet — the extension has not asked the '
  + 'chat-stasher host anything, so it has nothing to report.';

export function channelConnected(f: ChannelFacts): string {
  return 'Delivery channel: connected to the chat-stasher host'
    + ` — stage ${f.stage} · machine ${f.machine} · host version ${f.hostVersion}`
    + ` (checked ${stamp(f.at)}).`;
}

export function channelDisconnected(f: ChannelFacts): string {
  const why = f.kind ? `${f.reason} (${f.kind})` : (f.reason ?? 'unknown reason');
  const detail = f.detail && f.detail.length > 0 ? ` Detail: ${f.detail}.` : '';
  const stage = f.lastKnownStage ?? null;
  const stageLine = stage
    ? ` Last known stage: ${stage}.`
    : ' No stage path has ever been learned on this machine.';
  return 'Delivery channel: NOT connected to the chat-stasher host'
    + ` — reason: ${why} (checked ${stamp(f.at)}).`
    + detail
    + stageLine
    + ` Fix: ${fixCommand(stage)}`;
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

export const OUTBOX_EMPTY =
  'Outbox: empty — every capture so far has been acknowledged by the host.';

export interface OutboxFacts {
  pending: number;
  rejected: number;
  bytes: number;
  capacityBytes: number;
  full: boolean;
  rejectedKinds: ReadonlyArray<{ kind: string; count: number }>;
  rejectedSamples: ReadonlyArray<{ kind: string; detail: string }>;
}

export function outboxLine(f: OutboxFacts): string {
  const head = `Outbox: ${f.pending} waiting`
    + `, ${f.rejected} rejected`
    + ` — using ${formatBytes(f.bytes)} of ${formatBytes(f.capacityBytes)}.`;
  if (!f.full && f.rejected === 0) return head;
  const parts: string[] = [];
  if (f.full) {
    parts.push('The outbox is FULL: new captures are refused until the host takes what is '
      + 'already queued. Nothing queued was deleted.');
  }
  if (f.rejected > 0) {
    const kinds = f.rejectedKinds.map((k) => `${k.kind} × ${k.count}`).join(', ');
    parts.push(`Rejected by the host and kept (never retried): ${kinds}.`);
    for (const sample of f.rejectedSamples) {
      parts.push(`  · ${sample.kind}: ${sample.detail}`);
    }
  }
  return [head, ...parts].join('\n');
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

export const EXPORT_BUTTON_LABEL = 'Export undelivered captures';

export const EXPORT_EMPTY_NOTE =
  'Nothing to export: the outbox is empty, so there is no undelivered capture.';

export function exportNote(rec: { at: number; entries: number; bytes: number; filename: string }): string {
  return `Export: last written ${stamp(rec.at)} — ${rec.entries} capture(s),`
    + ` ${formatBytes(rec.bytes)}, file name ${rec.filename}.`
    + ' Exported captures stay in the outbox: the host will confirm them as duplicates'
    + ' once it is reachable again.';
}

export const EXPORT_NO_HISTORY = 'Export: no export file has been written yet.';

/** The popup shows this when the user presses the button and nothing was queued. */
export const EXPORT_NOTHING_QUEUED = 'Nothing to export.';

// ---------------------------------------------------------------------------
// Backfill pause
// ---------------------------------------------------------------------------

/**
 * Why backfill is stopped. §10: an undelivered backfill item keeps its debt
 * open and backfill pauses with a visible reason until a `hello` succeeds.
 */
export function backfillPaused(at: number, reason: string, detail?: string): string {
  return `Backfill: PAUSED — the host is unavailable (${reason}), first noticed ${stamp(at)}.`
    + (detail ? ` Detail: ${detail}.` : '')
    + ' The conversations still owed are untouched and will be picked up from where they'
    + ' stopped when the host answers again.';
}

// ---------------------------------------------------------------------------
// Badge
// ---------------------------------------------------------------------------

export function badgeTitle(pending: number, rejected: number, full: boolean): string {
  const parts = [`${pending} capture(s) waiting for the chat-stasher host`];
  if (rejected > 0) parts.push(`${rejected} rejected by the host`);
  if (full) parts.push('the outbox is full and refusing new captures');
  return `chat-stasher: ${parts.join(' · ')}`;
}
