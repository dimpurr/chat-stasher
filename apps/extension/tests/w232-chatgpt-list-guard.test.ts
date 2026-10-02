/**
 * W232 · **ChatGPT's offset-paged list had no "the parameter did not move" guard, so an
 * enumeration that stops advancing never ends and never says so.**
 *
 * ## The gap, stated against the code it lives in
 *
 * The repeat-page guard (`engine.ts`) asks one question: *did the parameter this engine
 * advances actually advance?* Until W232 it ran for exactly two kinds of plan —
 * `tokenMode`, and offset plans that declare `listOffsetInferred` (claude.ai):
 *
 *     const guardApplies = tokenMode || plan.listOffsetInferred === true;
 *
 * ChatGPT is neither. It pages by `offset`, and the engine advances that offset itself
 * (`state.enumCursor.offset += parsed.page.ids.length`), so the question applies to it as
 * much as to any other offset plan — it was simply never asked. The consequences are all
 * on the same side, and none of them is observable:
 *
 *  · ChatGPT's only termination signal is the **empty page** (`engine.ts`; the
 *    `offset >= total` branch was removed in W10 because `total` is not the account's
 *    size). A server that returns a non-empty page for every offset therefore ends the
 *    enumeration **never**;
 *  · a ChatGPT leg reads **one list page per tick** (`canBackfillDetail(plan) ⇒ 1`), so
 *    the loop is slow but unbounded: the same page is re-read every 1–2 minutes for as
 *    long as the scope runs, `enqueueDebts` adds nothing, and
 *  · every persisted state still reads as healthy: `complete` is false (nothing said it
 *    finished), `truncated` is undefined (nothing said it stopped), `halted` is null
 *    (nothing said it failed), and `offset` **grows** on every tick, so even a progress
 *    view shows movement.
 *
 * That is the failure this file reproduces. It is not hypothetical on this platform: the
 * sibling `gizmos/{id}/conversations` route silently ignores `offset` and paged by an
 * opaque cursor instead — measured by a competitor whose project export consequently
 * fetched only its first page (`chatgpt-exporter`, pionxzh
 * issue #341, fixed in commit `0e17703`). The same silent-ignore on the list route is the
 * one wire behaviour this leg cannot survive.
 *
 * ## What W232 does, and what it deliberately does not
 *
 * The guard's condition becomes "**every plan whose paging parameter this engine advances
 * itself**" — i.e. everything except cursor paging, where the cursor comes from the
 * response and the plan already has its own named outcomes (`cursor-missing`,
 * `has-more-missing`). The detector, the fingerprint, the halt and its wording are
 * unchanged; only which plans they run for.
 *
 * The outcome for a repeated page is the one W124b already decided for the same question
 * on another platform, and it is reused rather than re-argued: `halt('shape-changed')` —
 * permanent and traced — and deliberately neither `complete = true` (that would write
 * "the account was listed" over "we never got past page 1") nor a silent `break` (the
 * user would be told the backfill finished).
 *
 * 🔴 What this file does **not** claim: that any of this is happening on the owner's
 *    account. It is the mechanism that would make it happen silently, reproduced with
 *    synthetic pages and an injected http port — zero network, zero logged-in state.
 *    Whether the live endpoint ever repeats a page is a measurement, and it is on the
 *    read-only check list (`W232-OUT.md` §5), not in here.
 *
 * ## Red before green
 *
 * Every case in `describe('W232 · a page that repeats is a halt')` and the reset case is
 * written against the **unfixed** condition. On `0c47e6a` (before the one-line change)
 * they fail — the leg keeps going, `halted` stays null, and `enumCursor.complete` stays
 * false. `W232-OUT.md` §3 records the revert run and its output.
 */

import { describe, expect, it } from 'vitest';
import {
  listPageFingerprint,
  loadState,
  runBackfill as runBackfillRaw,
  type HttpResponse,
  type HttpPort,
} from '../lib/backfill/engine';
import { withChatGptLeaseIdentity } from './chatgpt-lease-fixtures';
const runBackfill = (options: Parameters<typeof runBackfillRaw>[0]) =>
  runBackfillRaw(options.platform === 'chatgpt' && options.http
    ? { ...options, http: withChatGptLeaseIdentity(options.http) }
    : options);
import { memoryStore, type BackfillStore } from '../lib/backfill/store';
import { replaceDebtSet } from '../lib/backfill/debt-store';
import { DEFAULT_LIST_LIMIT, CHATGPT_PLAN } from '../lib/backfill/enumerate';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const LIST_PATH = '/backend-api/conversations';
const SCOPE = 'default';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

/** A fixed clock, so nothing in these tests depends on wall time. */
function fixedClock(at: number): Clock {
  return { now: () => at, async sleep() { /* no waiting in these tests */ } };
}

/** One synthetic ChatGPT list page: `{items, limit, offset}`, ids only. */
function listPage(ids: readonly string[], offset = 0): string {
  return JSON.stringify({
    items: ids.map((id) => ({ id, title: 'synthetic-fixture', create_time: 0 })),
    limit: 100,
    offset,
    total: null,
  });
}

interface Backend {
  http: HttpPort;
  /** The `offset` of every list request, in order. */
  listOffsets: number[];
}

/** Answers the list by `offset`; **throws** on any path it was not given. */
function chatgptBackend(pages: Record<number, string>): Backend {
  const listOffsets: number[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    const u = new URL(url);
    if (u.pathname !== LIST_PATH) throw new Error(`unexpected path ${u.pathname}`);
    const offset = Number(u.searchParams.get('offset') ?? '0');
    listOffsets.push(offset);
    return { status: 200, text: pages[offset] ?? JSON.stringify({ items: [], limit: 100, offset, total: null }) };
  };
  return { http, listOffsets };
}

interface Seed {
  pending: string[];
  archived: string[];
  cursor: BackfillHeader['enumCursor'];
}

/** Write a pre-existing ChatGPT ledger: the debt set (the authority) plus its header. */
async function seed(store: BackfillStore, s: Seed): Promise<void> {
  await replaceDebtSet('chatgpt', SCOPE, {
    pending: s.pending,
    archived: s.archived,
    nextSeq: s.pending.length + s.archived.length + 1,
    times: new Map(),
  });
  const header: BackfillHeader = {
    v: 2,
    platform: 'chatgpt',
    scope: SCOPE,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: s.cursor,
    pendingCount: s.pending.length,
    archivedCount: s.archived.length,
    detailToday: { day: '2026-09-24', count: 0 },
    halted: null,
  };
  await store.save(stateKey('chatgpt', SCOPE), header);
}

function runChatgpt(
  store: BackfillStore,
  http: HttpPort,
  listLimit: number,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'chatgpt',
    origin: ORIGIN,
    scope: SCOPE,
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

describe('W232 · a page that repeats is a halt, not an enumeration that never ends', () => {
  it('a server that ignores `offset` returns page 1 again, and that is a halt', async () => {
    const store = memoryStore();
    const PAGE = ['c1-aaaa', 'c2-aaaa'];
    // Every offset gets the same page — the visible symptom of a backend that ignores
    // the parameter (the shape pionxzh measured on ChatGPT's sibling list route).
    const be = chatgptBackend({ 0: listPage(PAGE), 2: listPage(PAGE), 4: listPage(PAGE) });

    // Tick 1 · page 1 is read and recorded. Nothing is wrong yet.
    const first = await runChatgpt(store, be.http, 2);
    expect(first.halted).toBeNull();
    expect(be.listOffsets).toEqual([0]);
    expect(first.state.enumCursor.offset).toBe(2);
    expect(first.state.enumCursor.pageFingerprints).toEqual([await listPageFingerprint(PAGE)]);
    expect(first.state.enumCursor.complete).toBe(false);

    // Tick 2 · offset 2 hands back the page offset 0 already returned.
    // 🔴 Before W232 this ran on for every tick: no halt, no end, and no record saying so.
    const second = await runChatgpt(store, be.http, 2);
    expect(second.stopped).toBe('halted');
    expect(second.halted?.reason).toBe('shape-changed');
    expect(second.halted?.detail).toContain('did not advance');
    expect(second.halted?.detail).toContain('offset');
    // Not an ending: "the page repeated" must never be written as "the list is finished".
    expect(second.state.enumCursor.complete).toBe(false);
    expect(second.enumTruncated).toBeNull();
    expect(be.listOffsets).toEqual([0, 2]);
  });

  it('a LATER page repeated (not the first page) halts too', async () => {
    const store = memoryStore();
    // The reviewer's shape, which a first-page-only detector misses: A at 0, B at 2,
    // then B again at 4. B is neither the first page nor "already owed".
    const A = ['a1-aaaa', 'a2-aaaa'];
    const B = ['b1-aaaa', 'b2-aaaa'];
    const be = chatgptBackend({ 0: listPage(A), 2: listPage(B), 4: listPage(B) });

    expect((await runChatgpt(store, be.http, 2)).halted).toBeNull();
    const second = await runChatgpt(store, be.http, 2);
    expect(second.halted).toBeNull();
    expect(be.listOffsets).toEqual([0, 2]);
    expect(second.state.enumCursor.pageFingerprints).toEqual([
      await listPageFingerprint(A),
      await listPageFingerprint(B),
    ]);

    const third = await runChatgpt(store, be.http, 2);
    expect(third.stopped).toBe('halted');
    expect(third.halted?.reason).toBe('shape-changed');
    expect(third.halted?.detail).toContain('did not advance');
    expect(be.listOffsets).toEqual([0, 2, 4]);
    // Left where the repeated page was requested: the cursor does not pretend to advance.
    expect(third.state.enumCursor.offset).toBe(4);
    expect(third.state.enumCursor.complete).toBe(false);
  });

  it('a header written before this field existed starts recording from its own tick, and a repeat after that still halts', async () => {
    // 🔴 The compatibility case, and on this platform it is the *current* case rather
    //    than a historical one: `pageFingerprints` arrived with W124b for claude.ai, so
    //    every ChatGPT ledger on disk today carries no such field. A pass already in
    //    flight must therefore start recording where it is, not from a page it has
    //    forgotten — and a page already returned *before* the upgrade cannot be
    //    fingerprinted retroactively, which is stated rather than papered over.
    const store = memoryStore();
    // Mid-pass: two pages already read, nothing recorded.
    await seed(store, { pending: [], archived: [], cursor: { offset: 4, complete: false } });
    const C = ['c9-aaaa', 'c8-aaaa'];
    const be = chatgptBackend({ 4: listPage(C), 6: listPage(C) });

    const first = await runChatgpt(store, be.http, 2);
    expect(first.halted).toBeNull();
    expect(be.listOffsets).toEqual([4]);
    // Recorded from this tick on — the pages before the upgrade are not in the set.
    expect(first.state.enumCursor.pageFingerprints).toEqual([await listPageFingerprint(C)]);

    const second = await runChatgpt(store, be.http, 2);
    expect(second.stopped).toBe('halted');
    expect(second.halted?.reason).toBe('shape-changed');
    expect(second.halted?.detail).toContain('did not advance');
    expect(be.listOffsets).toEqual([4, 6]);
  });
});

describe('W232 · ordinary ChatGPT enumeration is unchanged by the guard', () => {
  it('three distinct pages and then the empty page: recorded, completed, no halt', async () => {
    const store = memoryStore();
    const P1 = ['p1-aaaa', 'p2-aaaa'];
    const P2 = ['p3-aaaa', 'p4-aaaa'];
    const P3 = ['p5-aaaa'];
    const be = chatgptBackend({ 0: listPage(P1), 2: listPage(P2), 4: listPage(P3) });

    const first = await runChatgpt(store, be.http, 2);
    const second = await runChatgpt(store, be.http, 2);
    const third = await runChatgpt(store, be.http, 2);
    expect([first.halted, second.halted, third.halted]).toEqual([null, null, null]);

    // 🔴 A SHORT page does not end a ChatGPT enumeration (no `listOffsetInferred`):
    //    short-page-inferred is Perplexity's and claude.ai's inference only. This is the
    //    pre-existing behaviour the guard must not disturb, and it is why page 3 is read
    //    even though it holds fewer rows than the limit.
    expect(third.state.enumCursor.complete).toBe(false);
    expect(third.state.enumCursor.truncated).toBeUndefined();

    // Tick 4 · only the empty page ends it.
    const fourth = await runChatgpt(store, be.http, 2);
    expect(fourth.halted).toBeNull();
    expect(fourth.state.enumCursor.complete).toBe(true);
    // Offsets advance by the number of rows the page handed over (2+2+1), so the empty
    // page is asked for at 5 — not at a multiple of the limit.
    expect(be.listOffsets).toEqual([0, 2, 4, 5]);

    // Every non-empty page of the pass was recorded, in order, and none repeated.
    const persisted = await loadState(store, 'chatgpt', SCOPE);
    expect(persisted.enumCursor.pageFingerprints).toEqual([
      await listPageFingerprint(P1),
      await listPageFingerprint(P2),
      await listPageFingerprint(P3),
    ]);
    // Nothing was duplicated and nothing was lost.
    expect([...fourth.state.pending].sort())
      .toEqual([...P1, ...P2, ...P3].sort());
  });

  it('an empty first page is still an ending — the guard does not turn it into a halt', async () => {
    const store = memoryStore();
    const be = chatgptBackend({});
    const report = await runChatgpt(store, be.http, 2);
    expect(report.halted).toBeNull();
    expect(report.state.enumCursor.complete).toBe(true);
    expect(report.state.enumCursor.pageFingerprints).toBeUndefined();
    expect(be.listOffsets).toEqual([0]);
  });
});

describe('W232 · the page size this leg asks for is inside the endpoint\'s measured maximum', () => {
  /**
   * 🔴 The measured fact this pins: `GET /backend-api/conversations` rejects
   * `limit > 100` with HTTP 422 (`value_error.number.not_le`). A 422 lands on
   * `haltReasonForStatus`, which is a **permanent** halt — so "raise the page size to
   * enumerate faster" is not a tuning change, it is a way to stop the leg for good.
   * The constant is correct today; this is the assertion that keeps it correct.
   */
  const CHATGPT_LIST_LIMIT_MAX = 100;

  it('the default page size is at most the endpoint maximum', () => {
    expect(DEFAULT_LIST_LIMIT).toBeLessThanOrEqual(CHATGPT_LIST_LIMIT_MAX);
  });

  it('the ChatGPT list URL carries that page size and nothing larger', () => {
    const url = new URL(CHATGPT_PLAN.listUrl(ORIGIN, 0, DEFAULT_LIST_LIMIT));
    expect(url.pathname).toBe(LIST_PATH);
    expect(Number(url.searchParams.get('limit'))).toBeLessThanOrEqual(CHATGPT_LIST_LIMIT_MAX);
    // The plan declares no page size of its own, so the default is what production sends.
    expect(CHATGPT_PLAN.listPageSize).toBeUndefined();
  });
});
