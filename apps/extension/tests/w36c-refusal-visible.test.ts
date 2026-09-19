/**
 * W36c · **A refusal that is written down but never rendered is not visible.**
 *
 * ## Why this is a file of its own
 *
 * W36b added two sentences to the popup, and both exist for the same reason: the
 * first real-Chrome acceptance saw a tick trace reading `{ran: true, reason:
 * 'ran'}` next to a storage layout that had not moved, and there was no sentence
 * anywhere that said the run had stopped. So
 *
 *   · `lastTickNote` appends `popup.lastTick.halted` when the trace carries a
 *     halt (`lib/popup-view.ts`), because a run that refuses a state record it
 *     cannot read **returns a report like any other** — `ran: true` is what
 *     `recordAlarmTick` stores for it, and leading with that alone is the
 *     reading that cost the acceptance days; and
 *   · `legacyMigrationNote` has a **refused** sentence, because "there was
 *     nothing to move" and "there was something and it would not move" are two
 *     different facts and only one of them means the user's ids are still
 *     sitting in a layout nothing will run against.
 *
 * Neither sentence had a test. Every other popup test stays green without them:
 * the happy-path cases never set `halted` or `refusal`, so a later edit can drop
 * the append, keep the whole suite green, and put the popup back to printing
 * "that tick really ran" over a refused ledger. `e2e/popup-migration.spec.ts`
 * checks the "Storage layout" line only after a **successful** move.
 *
 * ## What this file pins
 *
 * Both sentences, through the real front door (`renderPopup` — not the private
 * note functions, which is where a drop would actually happen). Each case also
 * asserts the sentence it must **not** be, so the two outcomes cannot collapse
 * into one another unnoticed.
 */

import { describe, it, expect } from 'vitest';
import { popupText, renderPopup, NO_FAILURES, type PopupModel } from '../lib/popup-view';
import type { BackfillTickRecord, LegacyMigration } from '../lib/backfill/alarm';

const AT = Date.parse('2026-09-19T09:15:00.000Z');

/** The detail a real refusal carries: the engine names the key and what was wrong with it, never a conversation body. */
const TICK_DETAIL = 'cs_backfill_v2:chatgpt:acct-w36 has no `enumCursor`';
const REFUSAL_DETAIL = 'the record at cs_backfill_v1:chatgpt:acct-w36 has no `pending` list';

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

/** The notes as one string, which is how they reach the screen (`popupText`). */
function noteText(m: Partial<PopupModel>): string {
  return renderPopup(model(m)).notes.join('\n');
}

describe('W36c · a refusal in the record is a refusal on the screen', () => {
  it('🔴 a tick trace carrying a halt prints the halt — not only "that tick really ran"', () => {
    // 🔴 The exact record the acceptance read, plus the field W36 added to it: the
    //    tick ran, and the run then stopped because it could not read the state.
    const trace: BackfillTickRecord = {
      at: AT,
      ran: true,
      reason: 'ran',
      targets: 1,
      stopped: 'halted',
      halted: 'state-unreadable',
      detail: TICK_DETAIL,
    };

    const notes = noteText({ lastTick: trace });

    // The head is still the head: this is an addition, not a replacement, and the
    // timestamped "it ran" fact must not disappear.
    expect(notes).toContain('that tick really ran');

    // 🔴 The half that was missing. Drop the append in lastTickNote and this is
    //    the assertion that goes red — the notes then stop after the head line.
    expect(notes).toContain('stopped before it could finish');
    expect(notes).toContain('state-unreadable');
    expect(notes).toContain(TICK_DETAIL);

    // …and it reaches the flattened text the popup actually renders.
    expect(popupText(renderPopup(model({ lastTick: trace })))).toContain('stopped before it could finish');
  });

  it('🔴 a refused pre-W18 sweep prints the refusal — not the "moved" sentence', () => {
    const refused: LegacyMigration = {
      found: 2,
      moved: 0,
      orphaned: 0,
      refusal: { reason: 'state-unreadable', detail: REFUSAL_DETAIL },
    };

    const notes = noteText({ legacyMigration: refused });

    // 🔴 The three facts the refused sentence alone carries: how many were found,
    //    how many of those did not move, and that they are still there.
    expect(notes).toContain('2 older (pre-W18) backfill record(s) were found and only 0 could be moved');
    expect(notes).toContain('still in storage, untouched');
    expect(notes).toContain('will not run against them');
    expect(notes).toContain('Reason: state-unreadable');
    expect(notes).toContain(REFUSAL_DETAIL);

    // 🔴 **And it must not be the completed-repair sentence.** Both branches live
    //    in `legacyMigrationNote`; if the refusal branch is dropped the function
    //    falls through to the "moved" wording, which says the opposite thing
    //    about the same two records.
    expect(notes).not.toContain('this page moved');

    // The other side of the same coin, so the two sentences are pinned **apart**
    // and neither can be satisfied by the other's text.
    const moved: LegacyMigration = { found: 2, moved: 2, orphaned: 0, refusal: null };
    const movedNotes = noteText({ legacyMigration: moved });
    expect(movedNotes).toContain('this page moved 2 older (pre-W18) backfill record(s)');
    expect(movedNotes).not.toContain('could be moved');
  });
});
