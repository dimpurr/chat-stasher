/**
 * W124 / W124b · The Claude web backfill halted `shape-changed`, and the repeat-page
 * guard was the wrong detector.
 *
 * ## The measured failure this file reproduces
 * On the live Claude scope the list is 218 rows and the page size is 50. The W98
 * migration reset the cursor and re-listed; offsets 0/50/100/150 re-added the ids
 * the pre-W92 walk had dropped, and **offset 200 — the 18-row tail the pre-W92 run
 * never reached, still `pending` — was judged by the old guard as "every id already
 * owed ⇒ the offset parameter did not move"** and wrote a permanent `shape-changed`
 * (`nm/W124-OUT.md` §1.0-1.2). Three live GETs at offsets 0/100/200 returned
 * pairwise-disjoint pages, so the parameter *was* advancing; the guard's premise was
 * simply false during a re-enumeration.
 *
 * W124 first fixed this by comparing a later page with the *first* page of the pass.
 * A read-only review found the remaining hole: a server that returns page A, then
 * page B, then keeps returning B for larger offsets is never caught, because B is
 * not the first page. W124b records the fingerprint (sha256 of the sorted id list) of
 * **every** non-empty page of the pass and halts when a page repeats any of them.
 *
 * ## Why the scenarios below are the observed shape, scaled down
 * A list page has a fixed size for a plan, and a short page ends the enumeration
 * (`listOffsetInferred`). The live shape is "a `pending` tail returned at an offset
 * past the already-recovered ids"; the fixtures use a 2/3-row page and a small fake
 * list so the same facts are exercised: a first page, a later page (owed or not), a
 * repeat, and the real ending. Every value is invented, the http port throws on any
 * path it was not given, and no network is touched.
 *
 * 🔴 Red-before-green: with the W124 guard (`every id on the page is on the first
 *    page`), the W98 re-list scenarios halt `shape-changed` at the tail page instead
 *    of finishing. With the pre-W124 guard (`every id is in archived ∪ pending`) the
 *    same two halt. The W124b-specific case (a *later* page repeated) is the one both
 *    earlier guards miss. `nm/W124b-OUT.md` records the revert run.
 */

import { describe, expect, it } from 'vitest';
import {
  loadState,
  listPageFingerprint,
  runBackfill,
  type HttpResponse,
  type HttpPort,
} from '../lib/backfill/engine';
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { replaceDebtSet } from '../lib/backfill/debt-store';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://claude.ai';
/** A synthetic organization id in the shape `isClaudeOrgId` accepts. */
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const LIST_PATH = `/api/organizations/${ORG}/chat_conversations`;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fixed clock, so a migration timestamp is a known number. */
function fixedClock(at: number): Clock {
  return { now: () => at, async sleep() { /* no waiting in these tests */ } };
}

/** A bare-array list page, as claude.ai returns it. */
function listPage(ids: readonly string[]): string {
  return JSON.stringify(ids.map((uuid) => ({ uuid, name: `synthetic-${uuid}` })));
}

interface Backend {
  http: HttpPort;
  /** The `offset` of every list request, in order. */
  listOffsets: number[];
}

/** Answers the list by `offset`; **throws** on any path it was not given. */
function claudeBackend(pages: Record<number, string>): Backend {
  const listOffsets: number[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    const u = new URL(url);
    if (u.pathname !== LIST_PATH) throw new Error(`unexpected path ${u.pathname}`);
    const offset = Number(u.searchParams.get('offset') ?? '0');
    listOffsets.push(offset);
    return { status: 200, text: pages[offset] ?? '[]' };
  };
  return { http, listOffsets };
}

interface Seed {
  pending: string[];
  archived: string[];
  cursor: BackfillHeader['enumCursor'];
  /** Omitted ⇒ the W98 migration has not run here, so this run resets the cursor. */
  reenumerated?: Record<string, number>;
}

/** Write a pre-existing Claude ledger: the debt set (the authority) plus its header. */
async function seed(store: BackfillStore, scope: string, s: Seed): Promise<void> {
  await replaceDebtSet('claude', scope, {
    pending: s.pending,
    archived: s.archived,
    nextSeq: s.pending.length + s.archived.length + 1,
  });
  const header: BackfillHeader = {
    v: 2,
    platform: 'claude',
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: s.cursor,
    pendingCount: s.pending.length,
    archivedCount: s.archived.length,
    detailToday: { day: '2026-09-24', count: 0 },
    halted: null,
    ...(s.reenumerated === undefined ? {} : { reenumerated: s.reenumerated }),
  };
  await store.save(stateKey('claude', scope), header);
}

function runClaude(
  store: BackfillStore,
  http: HttpPort,
  scope: string,
  listLimit: number,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'claude',
    origin: ORIGIN,
    scope,
    store,
    http,
    clock: fixedClock(Date.parse('2026-09-24T00:00:00.000Z')),
    pace: NO_WAIT,
    random: () => 0,
    listLimit,
    // No bodies: this file is about the list guard and the debt set.
    maxDetails: 0,
    ...extra,
  });
}

describe('W124 · the repeat-page guard does not fire on an already-owed re-list page', () => {
  it('a legacy mid-pass header (no pageFingerprints): the fully-owed tail page does not halt, the pass ends', async () => {
    const scope = ORG;
    const store = memoryStore();
    // Exactly the live state after the partial re-list: cursor at the tail page, the
    // tail ids still owed, and no pageFingerprints because the header predates W124b.
    // The W98 marker is present, so this run does NOT reset the cursor.
    const TAIL = ['t1-tail', 't2-tail'];
    await seed(store, scope, {
      pending: TAIL,
      archived: [],
      cursor: { offset: 3, complete: false },
      reenumerated: { 'claude-detail-walk-w92': Date.parse('2026-09-24T00:00:00.000Z') },
    });
    // Page size 3; the page at offset 3 is the short, fully-owed tail; offset 5 ends it.
    const be = claudeBackend({ 3: listPage(TAIL) });

    const report = await runClaude(store, be.http, scope, 3);

    // 🔴 The old guard halted here (`shape-changed`); the fix reads the page.
    expect(report.stopped).not.toBe('halted');
    expect(report.halted).toBeNull();
    expect(be.listOffsets).toEqual([3]);
    // The short page is the plan's own inference that the listing ends — untouched.
    expect(report.state.enumCursor.truncated).toBe('short-page-inferred');
    expect(report.state.enumCursor.offset).toBe(5);
    // A header that predates W124b starts recording from this tick, so a repeat from
    // here on would still be caught.
    expect(report.state.enumCursor.pageFingerprints).toEqual([await listPageFingerprint(TAIL)]);
    // Nothing was duplicated and nothing was lost: the tail was already owed.
    expect([...report.state.pending].sort()).toEqual([...TAIL].sort());
    expect(report.newDebts).toBe(0);
  });

  it('the W98 re-list records each page and a fully-owed tail is not a repeat of the first page, so the pass completes', async () => {
    const scope = ORG;
    const store = memoryStore();
    // The migration has not run here: this run resets the cursor and re-lists.
    // `pending` holds only the never-dropped tail; the first page's ids were dropped
    // by the pre-W92 walk and come back through enqueueDebts.
    const TAIL = ['t1-tail', 't2-tail'];
    const DROPPED = ['f1-dropped', 'f2-dropped'];
    await seed(store, scope, {
      pending: TAIL,
      archived: [],
      cursor: { offset: 4, complete: true },
    });
    const be = claudeBackend({ 0: listPage(DROPPED), 2: listPage(TAIL) });

    // Page size 2, one page per tick (a body-bearing plan).
    const first = await runClaude(store, be.http, scope, 2);
    expect(be.listOffsets).toEqual([0]);
    expect(first.state.enumCursor.offset).toBe(2);
    expect(first.stopped).not.toBe('halted');

    // Tick 2: the tail page — every id owed, and NOT a repeat of the first page.
    // 🔴 This is the assertion the old guard failed: it halted here.
    const second = await runClaude(store, be.http, scope, 2);
    expect(second.halted).toBeNull();
    expect(second.stopped).not.toBe('halted');
    expect(be.listOffsets).toEqual([0, 2]);
    expect(second.state.enumCursor.offset).toBe(4);
    // The mechanism the fix uses: both pages of the pass really were recorded, in
    // order, and no fingerprint repeats.
    expect(second.state.enumCursor.pageFingerprints).toEqual([
      await listPageFingerprint(DROPPED),
      await listPageFingerprint(TAIL),
    ]);

    // Tick 3: the empty page is the real ending.
    const third = await runClaude(store, be.http, scope, 2);
    expect(third.halted).toBeNull();
    expect(third.state.enumCursor.complete).toBe(true);
    expect(be.listOffsets).toEqual([0, 2, 4]);

    // The re-list did its job: the dropped ids are owed, nothing duplicated.
    expect([...third.state.pending].sort()).toEqual([...DROPPED, ...TAIL].sort());
    const persisted = await loadState(store, 'claude', scope);
    expect(persisted.enumCursor.pageFingerprints).toEqual([
      await listPageFingerprint(DROPPED),
      await listPageFingerprint(TAIL),
    ]);
  });

  it('a server that really ignores the offset returns the first page again, and that IS a halt', async () => {
    const scope = ORG;
    const store = memoryStore();
    // Every offset gets the same page — the symptom of a backend that ignores `offset`.
    const be = claudeBackend({ 0: listPage(['x1', 'x2']), 2: listPage(['x1', 'x2']) });
    const first = await runClaude(store, be.http, scope, 2);
    expect(first.state.enumCursor.offset).toBe(2);
    expect(first.state.enumCursor.pageFingerprints).toEqual([await listPageFingerprint(['x1', 'x2'])]);
    const between = await loadState(store, 'claude', scope);
    expect(between.enumCursor.offset).toBe(2);
    expect(between.enumCursor.pageFingerprints).toEqual([await listPageFingerprint(['x1', 'x2'])]);

    const second = await runClaude(store, be.http, scope, 2);
    expect(second.stopped).toBe('halted');
    expect(second.halted?.reason).toBe('shape-changed');
    expect(second.halted?.detail).toContain('did not advance');
    // Not an ending: the cursor is left where it was.
    expect(second.state.enumCursor.complete).toBe(false);
    expect(second.enumTruncated).toBeNull();
    expect(be.listOffsets).toEqual([0, 2]);
  });

  it('a later page repeated (not the first page) halts instead of enumerating forever', async () => {
    const scope = ORG;
    const store = memoryStore();
    // The reviewer's shape: page A at offset 0, page B at offset 2, then the full page
    // B again at offset 4. B is neither the first page nor "already owed" on the W124
    // pass (fresh scope), so only a fingerprint of *every* page catches it.
    const A = ['a1-first', 'a2-first'];
    const B = ['b1-later', 'b2-later'];
    const be = claudeBackend({ 0: listPage(A), 2: listPage(B), 4: listPage(B) });

    const first = await runClaude(store, be.http, scope, 2);
    expect(first.halted).toBeNull();
    expect(be.listOffsets).toEqual([0]);
    const second = await runClaude(store, be.http, scope, 2);
    expect(second.halted).toBeNull();
    expect(be.listOffsets).toEqual([0, 2]);
    expect(second.state.enumCursor.offset).toBe(4);
    expect(second.state.enumCursor.pageFingerprints).toEqual([
      await listPageFingerprint(A),
      await listPageFingerprint(B),
    ]);

    // Tick 3: offset 4 hands B back. Under the W124 guard this was never a repeat
    // (B is not the first page) and the offset would keep growing with no ending.
    const third = await runClaude(store, be.http, scope, 2);
    expect(third.stopped).toBe('halted');
    expect(third.halted?.reason).toBe('shape-changed');
    expect(third.halted?.detail).toContain('did not advance');
    expect(be.listOffsets).toEqual([0, 2, 4]);
    // Not an ending: the cursor is left where the repeated page was requested.
    expect(third.state.enumCursor.complete).toBe(false);
    expect(third.state.enumCursor.offset).toBe(4);
  });

  it('a reset clears the remembered fingerprints with the rest of the cursor', async () => {
    const scope = ORG;
    const store = memoryStore();
    const DROPPED = ['f1-dropped', 'f2-dropped'];
    await seed(store, scope, {
      pending: [],
      archived: [],
      cursor: { offset: 2, complete: true, pageFingerprints: ['stale-1', 'stale-2'] },
    });
    const be = claudeBackend({ 0: listPage(DROPPED) });
    const report = await runClaude(store, be.http, scope, 2);
    // The migration reset replaced the whole cursor, so the stale fingerprints are gone
    // and this pass's own page is recorded instead.
    expect(report.state.enumCursor.pageFingerprints).toEqual([await listPageFingerprint(DROPPED)]);
    expect(be.listOffsets).toEqual([0]);
  });

  it('the fingerprint is order-independent: the same id set in any order is the same page', async () => {
    expect(await listPageFingerprint(['b', 'a'])).toBe(await listPageFingerprint(['a', 'b']));
    expect(await listPageFingerprint(['a', 'b'])).not.toBe(await listPageFingerprint(['a', 'c']));
  });
});
