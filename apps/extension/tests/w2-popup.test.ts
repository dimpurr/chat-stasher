/**
 * W2 · The four new things the popup says: channel status, outbox, backfill pause, export.
 *
 * 🔴 Every one of them states only **facts that already happened** (timestamp included); not one is
 * "probably fine":
 *    · channel: what the last `hello` was told (stage / machine / version), or a named reason plus
 *      the fix command;
 *    · outbox: how many are waiting, how many were rejected (with the kinds and a detail summary),
 *      how much capacity is used;
 *    · pause: where backfill stopped because the host is unreachable, and that no debt was lost;
 *    · export: the button word and its visibility, and when the last export was written.
 *
 * Every one of these lines lives in the catalog (lib/ui-strings.ts resolves it through
 * lib/i18n.ts), and the popup's older lines live there too now — this file owns the W2 additions
 * and asserts them, it does not own the wording.
 */

import { describe, it, expect } from 'vitest';
import {
  channelLine,
  exportLine,
  outboxLine,
  pauseLine,
  popupText,
  renderPopup,
  summarizeOutbox,
  MAX_REJECTED_SAMPLES,
  NO_FAILURES,
  type PopupModel,
} from '../lib/popup-view';
import { OUTBOX_CAPACITY_BYTES, type OutboxEntry } from '../lib/outbox';
import * as ui from '../lib/ui-strings';

const AT = Date.parse('2026-09-12T21:47:03.000Z');

function model(overrides: Partial<PopupModel> = {}): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    ...overrides,
  };
}

function entry(overrides: Partial<OutboxEntry> = {}): OutboxEntry {
  return {
    sha256: 'a'.repeat(64),
    name: 'chatgpt-a.json',
    payload: '{"x":1}',
    bytes: 7,
    enqueuedAt: 1,
    attempts: 0,
    lastError: null,
    lastAttemptAt: null,
    state: 'pending',
    ...overrides,
  };
}

// ===========================================================================
// Delivery channel
// ===========================================================================
describe('W2-POPUP · the delivery channel', () => {
  it('connected ⇒ it states stage / machine / host version plus when it asked', () => {
    const line = channelLine(model({
      nativeHost: { at: AT, ok: true, stage: '/Users/me/stage', machine: 'mac-1', hostVersion: '0.3.0' },
    }));
    console.log('[W2-POPUP] connected:', line);
    expect(line).toContain('connected');
    expect(line).toContain('/Users/me/stage');
    expect(line).toContain('mac-1');
    expect(line).toContain('0.3.0');
    expect(line).toContain(ui.stamp(AT));
    // A healthy-path guard: once connected, the fix command must not appear.
    expect(line).not.toContain('install-native-host');
  });

  it('🔴 not connected ⇒ a named reason plus the fix command (using the last known stage)', () => {
    const line = channelLine(model({
      nativeHost: {
        at: AT, ok: false, reason: 'send-failed',
        detail: 'Specified native messaging host not found.',
        lastKnownStage: '/Users/me/stage',
      },
    }));
    console.log('[W2-POPUP] not connected:', line);
    expect(line).toContain('NOT connected');
    expect(line).toContain('send-failed');
    expect(line).toContain('chat-stasher install-native-host --stage /Users/me/stage');
    expect(line).toContain('Last known stage');
  });

  it('never asked ⇒ it says it was never asked (no guessing "probably fine")', () => {
    const line = channelLine(model({ nativeHost: null }));
    console.log('[W2-POPUP] never asked:', line);
    expect(line).toBe(ui.channelNoCheck());
    expect(line).toContain('no host check has run yet');
    // It may not even say "not connected" — we do not know whether it is.
    expect(line).not.toContain('NOT connected');
  });

  it('never connected and no stage on record ⇒ the fix command carries the placeholder rather than an invented path', () => {
    const line = channelLine(model({ nativeHost: { at: AT, ok: false, reason: 'timeout' } }));
    expect(line).toContain('chat-stasher install-native-host --stage <path-to-your-stage-dir>');
  });
});

// ===========================================================================
// Outbox
// ===========================================================================
describe('W2-POPUP · the outbox', () => {
  it('empty and not full ⇒ the line does not appear (then "nothing is waiting" is the whole truth)', () => {
    expect(outboxLine(model({ outbox: { pending: 0, rejected: 0, bytes: 0, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] } }))).toBeNull();
    expect(outboxLine(model())).toBeNull();
  });

  it('N waiting ⇒ it states the waiting count, the rejected count and the capacity used', () => {
    const line = outboxLine(model({
      outbox: {
        pending: 3, rejected: 0, bytes: 1024 * 1024, capacityBytes: OUTBOX_CAPACITY_BYTES,
        full: false, rejectedKinds: [], rejectedSamples: [],
      },
    }))!;
    console.log('[W2-POPUP] 3 waiting:', line);
    expect(line).toContain('3 waiting');
    expect(line).toContain('0 rejected');
    expect(line).toContain('1.0 MiB');
    expect(line).toContain('256.0 MiB');
  });

  it('🔴 something rejected ⇒ a per-kind count plus a per-entry detail summary', () => {
    const line = outboxLine(model({
      outbox: {
        pending: 1, rejected: 2, bytes: 10, capacityBytes: 100, full: false,
        rejectedKinds: [{ kind: 'invalid-bundle', count: 2 }],
        rejectedSamples: [
          { kind: 'invalid-bundle', detail: 'nack:invalid-bundle' },
          { kind: 'invalid-bundle', detail: 'nack:invalid-bundle' },
        ],
      },
    }))!;
    console.log('[W2-POPUP] with rejections:', line);
    expect(line).toContain('2 rejected');
    expect(line).toContain('invalid-bundle × 2');
    expect(line).toContain('never retried');
    expect(line).toContain('nack:invalid-bundle');
  });

  it('🔴 full ⇒ it says outright that new ones are refused, and that nothing already queued was deleted', () => {
    const line = outboxLine(model({
      outbox: {
        pending: 5, rejected: 0, bytes: 100, capacityBytes: 100, full: true,
        rejectedKinds: [], rejectedSamples: [],
      },
    }))!;
    expect(line).toContain('FULL');
    expect(line).toContain('Nothing queued was deleted');
  });

  it('🔴 unreadable ⇒ it says it is unreadable, never treating it as empty', () => {
    const line = outboxLine(model({ outbox: null }))!;
    console.log('[W2-POPUP] unreadable:', line);
    expect(line).toContain('unreadable');
    expect(line).toContain('cannot say what is queued');
  });

  it('summarizeOutbox counts correctly, and the samples are capped', () => {
    const entries = [
      entry({ sha256: '1'.repeat(64), state: 'pending', bytes: 10 }),
      entry({ sha256: '2'.repeat(64), state: 'rejected', bytes: 20, rejectKind: 'config', lastError: 'nack:config' }),
      entry({ sha256: '3'.repeat(64), state: 'rejected', bytes: 30, rejectKind: 'config', lastError: 'nack:config' }),
      ...Array.from({ length: 10 }, (_, i) => entry({
        sha256: String(i).padStart(64, '7'), state: 'rejected' as const, bytes: 5,
        rejectKind: 'invalid-bundle', lastError: 'nack:invalid-bundle',
      })),
    ];
    const out = summarizeOutbox(entries, 1000);
    console.log('[W2-POPUP] summary:', { ...out, rejectedSamples: out.rejectedSamples.length });
    expect(out.pending).toBe(1);
    expect(out.rejected).toBe(12);
    expect(out.bytes).toBe(10 + 20 + 30 + 50);
    expect(out.rejectedKinds).toEqual([
      { kind: 'invalid-bundle', count: 10 },
      { kind: 'config', count: 2 },
    ]);
    expect(out.rejectedSamples).toHaveLength(MAX_REJECTED_SAMPLES);
    expect(out.full).toBe(false);
  });

  it('🔴 the summary carries no payload / URL / conversation body', () => {
    const out = summarizeOutbox([
      entry({
        state: 'rejected', rejectKind: 'invalid-bundle', lastError: 'nack:invalid-bundle',
        payload: '{"secret":"synthetic conversation body"}',
      }),
    ]);
    const blob = JSON.stringify(out);
    expect(blob).not.toContain('synthetic conversation body');
    expect(blob).not.toContain('secret');
  });
});

// ===========================================================================
// Backfill pause
// ===========================================================================
describe('W2-POPUP · the backfill pause', () => {
  it('paused ⇒ it states the reason, when it was first noticed, and that no debt was moved', () => {
    const line = pauseLine(model({
      block: 'host-paused',
      hostPause: { reason: 'host-unavailable', at: AT, detail: 'timeout' },
    }))!;
    console.log('[W2-POPUP] paused:', line);
    expect(line).toContain('PAUSED');
    expect(line).toContain('host-unavailable');
    expect(line).toContain(ui.stamp(AT));
    expect(line).toContain('untouched');
  });

  it('not paused ⇒ the line does not appear', () => {
    expect(pauseLine(model({ hostPause: null }))).toBeNull();
    expect(pauseLine(model())).toBeNull();
  });
});

// ===========================================================================
// Export
// ===========================================================================
describe('W2-POPUP · export', () => {
  it('something undelivered ⇒ the button appears; everything delivered ⇒ it does not', () => {
    const withPending = renderPopup(model({
      outbox: { pending: 1, rejected: 0, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    }));
    expect(withPending.exportFile.visible).toBe(true);
    expect(withPending.exportFile.label).toBe(ui.exportButtonLabel());

    const withRejected = renderPopup(model({
      outbox: { pending: 0, rejected: 1, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [{ kind: 'config', count: 1 }], rejectedSamples: [] },
    }));
    expect(withRejected.exportFile.visible).toBe(true);

    const empty = renderPopup(model({
      outbox: { pending: 0, rejected: 0, bytes: 0, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    }));
    expect(empty.exportFile.visible).toBe(false);
  });

  it('🔴 the last export\'s time and file name must be visible; never exported, and it says so', () => {
    expect(exportLine(model())).toBe(ui.exportNoHistory());
    const line = exportLine(model({
      lastExport: { at: AT, entries: 4, bytes: 2048, filename: 'chat-stasher-export-20260912T214703Z.jsonl' },
    }));
    console.log('[W2-POPUP] the last export:', line);
    expect(line).toContain(ui.stamp(AT));
    expect(line).toContain('4 capture(s)');
    expect(line).toContain('chat-stasher-export-20260912T214703Z.jsonl');
    // Exporting does **not** delete entries — that sentence has to be somewhere the user can see it.
    expect(line).toContain('stay in the outbox');
  });

  it('the button is visible in the flattened text (otherwise "did it appear" cannot be asserted)', () => {
    const out = popupText(renderPopup(model({
      outbox: { pending: 2, rejected: 0, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    })));
    expect(out).toContain(`[Button] ${ui.exportButtonLabel()}`);
    expect(out).toContain('2 waiting');
  });
});

// ===========================================================================
// Coexisting with C18's older red lines
// ===========================================================================
describe('W2-POPUP · the new wording must not step on C18\'s older red lines', () => {
  it('🔴 not one of W2\'s new lines contains a percent sign (the progress rule applies to the whole page)', () => {
    const m = model({
      block: 'host-paused',
      hostPause: { reason: 'host-unavailable', at: AT },
      nativeHost: { at: AT, ok: false, reason: 'timeout', lastKnownStage: '/s' },
      outbox: {
        pending: 9, rejected: 3, bytes: 12345678, capacityBytes: OUTBOX_CAPACITY_BYTES, full: true,
        rejectedKinds: [{ kind: 'config', count: 3 }],
        rejectedSamples: [{ kind: 'config', detail: 'nack:config' }],
      },
      lastExport: { at: AT, entries: 12, bytes: 999, filename: 'f.jsonl' },
    });
    const lines = [channelLine(m), outboxLine(m), pauseLine(m), exportLine(m)].filter(Boolean) as string[];
    expect(lines).toHaveLength(4);
    for (const line of lines) expect([line, line.includes('%')]).toEqual([line, false]);
    // Along the way: the whole popupText contains no percent sign with everything present either.
    expect(popupText(renderPopup(m))).not.toContain('%');
  });

  it('🔴 no time promise appears in the new wording (we have no rate model)', () => {
    const m = model({
      nativeHost: { at: AT, ok: false, reason: 'timeout' },
      outbox: { pending: 1, rejected: 0, bytes: 1, capacityBytes: 100, full: false, rejectedKinds: [], rejectedSamples: [] },
    });
    const lines = [channelLine(m), outboxLine(m), exportLine(m)].filter((l): l is string => l !== null);
    expect(lines).toHaveLength(3);
    for (const line of lines) {
      for (const promise of ['in a few minutes', 'shortly', 'soon', 'ETA', 'estimated', 'time remaining']) {
        expect([line, line.includes(promise)]).toEqual([line, false]);
      }
    }
  });
});
