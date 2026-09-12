/**
 * The wordings for everything outside the popup's own state machine: the
 * delivery channel, the outbox, the export button, the backfill pause and the
 * toolbar badge title.
 *
 * 🔴 Every one of these is now a *builder*, not a constant, and every builder
 *    goes through `t` (lib/i18n.ts). Two reasons, in order of importance:
 *
 *    1. The popup can switch language while it is open, so the text has to be
 *       resolved at paint time. A module-level string constant would be frozen
 *       at import time, in whatever language happened to be active then.
 *    2. The wording lives in locales/en.yml + locales/zh_CN.yml, next to every
 *       other user-visible string, so a translator has one file to read instead
 *       of one file plus this one.
 *
 * Nothing here computes state. Each builder takes the facts it was handed and
 * states them; a missing fact becomes a missing fact in the sentence, never a
 * plausible default. That rule survived the move to keys intact — the
 * placeholders are the same facts, and a missing one still has to be spelled
 * out (e.g. `channelDisconnected` says "no stage path has ever been learned"
 * rather than quietly interpolating an empty string).
 */

import { t } from './i18n';
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
  if (typeof at !== 'number' || !Number.isFinite(at)) return t('common.unknownTime');
  return `${new Date(at).toISOString().replace('T', ' ').slice(0, 19)} UTC`;
}

/**
 * Bytes → a short human quantity. Used for outbox capacity, never for rates.
 * The unit symbols are not translated: KiB/MiB/GiB are the same string in every
 * language this catalog offers, and inventing localised unit names would make
 * the number harder, not easier, to compare against the capacity we chose.
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return t('common.unknownSize');
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

export function channelNoCheck(): string {
  return t('channel.noCheck');
}

export function channelConnected(f: ChannelFacts): string {
  return t('channel.connected', {
    stage: f.stage,
    machine: f.machine,
    hostVersion: f.hostVersion,
    at: stamp(f.at),
  });
}

export function channelDisconnected(f: ChannelFacts): string {
  const why = f.kind
    ? `${f.reason} (${f.kind})`
    : (f.reason ?? t('channel.reason.unknown'));
  const stage = f.lastKnownStage ?? null;
  return t('channel.disconnected.head', { why, at: stamp(f.at) })
    + (f.detail && f.detail.length > 0 ? t('channel.disconnected.detail', { detail: f.detail }) : '')
    + (stage
      ? t('channel.disconnected.stageKnown', { stage })
      : t('channel.disconnected.stageNever'))
    + t('channel.disconnected.fix', { command: fixCommand(stage) });
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

export function outboxEmpty(): string {
  return t('outbox.empty');
}

export interface OutboxFacts {
  pending: number;
  rejected: number;
  bytes: number;
  capacityBytes: number;
  full: boolean;
  rejectedKinds: ReadonlyArray<{ kind: string; count: number }>;
  rejectedSamples: ReadonlyArray<{ kind: string; detail: string }>;
}

/**
 * Data-layer markers `summarizeOutbox` puts in place of a value it never
 * learned. They are plain strings in the (pure, tested) summarising function,
 * and are mapped to their translated wording here, at paint time — so the
 * "we never recorded a detail" case reads as that sentence in either language
 * instead of leaking an English sentinel into a Chinese screen.
 */
export const OUTBOX_KIND_UNKNOWN = 'unknown';
export const OUTBOX_DETAIL_MISSING = 'no detail recorded';

export function outboxLine(f: OutboxFacts): string {
  const head = t('outbox.head', {
    pending: f.pending,
    rejected: f.rejected,
    bytes: formatBytes(f.bytes),
    capacity: formatBytes(f.capacityBytes),
  });
  if (!f.full && f.rejected === 0) return head;
  const parts: string[] = [];
  if (f.full) parts.push(t('outbox.full'));
  if (f.rejected > 0) {
    const kinds = f.rejectedKinds
      .map((k) => `${k.kind === OUTBOX_KIND_UNKNOWN ? t('outbox.unknownKind') : k.kind} × ${k.count}`)
      .join(', ');
    parts.push(t('outbox.rejected', { kinds }));
    for (const sample of f.rejectedSamples) {
      parts.push(t('outbox.sample', {
        kind: sample.kind === OUTBOX_KIND_UNKNOWN ? t('outbox.unknownKind') : sample.kind,
        detail: sample.detail === OUTBOX_DETAIL_MISSING ? t('outbox.noDetail') : sample.detail,
      }));
    }
  }
  return [head, ...parts].join('\n');
}

export function outboxUnreadable(): string {
  return t('outbox.unreadable');
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

export function exportButtonLabel(): string {
  return t('export.buttonLabel');
}

export function exportEmptyNote(): string {
  return t('export.emptyNote');
}

export function exportNote(rec: { at: number; entries: number; bytes: number; filename: string }): string {
  return t('export.note', {
    at: stamp(rec.at),
    entries: rec.entries,
    bytes: formatBytes(rec.bytes),
    filename: rec.filename,
  });
}

export function exportNoHistory(): string {
  return t('export.noHistory');
}

/** The popup shows this when the user presses the button and nothing was queued. */
export function exportNothingQueued(): string {
  return t('export.nothingQueued');
}

export function exportUnreadable(): string {
  return t('export.unreadable');
}

// ---------------------------------------------------------------------------
// Backfill pause
// ---------------------------------------------------------------------------

/**
 * Why backfill is stopped. §10: an undelivered backfill item keeps its debt
 * open and backfill pauses with a visible reason until a `hello` succeeds.
 */
export function backfillPaused(at: number, reason: string, detail?: string): string {
  return t('backfill.paused.head', { reason, at: stamp(at) })
    + (detail ? t('backfill.paused.detail', { detail }) : '')
    + t('backfill.paused.tail');
}

// ---------------------------------------------------------------------------
// Badge
// ---------------------------------------------------------------------------

export function badgeTitle(pending: number, rejected: number, full: boolean): string {
  const parts = [t('badge.waiting', { pending })];
  if (rejected > 0) parts.push(t('badge.rejected', { rejected }));
  if (full) parts.push(t('badge.full'));
  return t('badge.title', { parts: parts.join(' · ') });
}

export function badgeUnreadable(): string {
  return t('badge.unreadable');
}
