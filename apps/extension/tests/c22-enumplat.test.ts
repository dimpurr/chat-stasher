/**
 * C22 · The backfill leg's platform enumeration table.
 *
 * 🔴 All fixtures are synthetic: not one line touches a real platform endpoint, there is no
 * logged-in state and no real conversation body. The http port is always injected; without one
 * it is notWiredHttp (calling it throws).
 *
 * Three criteria, corresponding to three outcomes that **mean completely different things to a user**:
 *   1. a backfillable platform ⇒ a synthetic response ⇒ N (≥2) rows enumerated ⇒ into the debt set;
 *   2. a shape mismatch ⇒ halt('shape-changed') with a trace, never in silence;
 *   3. a platform not supported yet ⇒ halt('unsupported-platform'), **and not one request may go
 *      out** — it has to be a distinguishable outcome from "0 rows enumerated" (= you really have
 *      no history).
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, type HttpResponse } from '../lib/backfill/engine';
import { t } from '../lib/i18n';
import { memoryStore } from '../lib/backfill/store';
import { PLATFORMS } from '../lib/contract';
import {
  BACKFILL_SUPPORTED_PLATFORMS,
  BACKFILL_UNSUPPORTED,
  BACKFILL_UNSUPPORTED_PLATFORMS,
  backfillPlanFor,
  unsupportedBackfillFor,
} from '../lib/backfill/enumerate';
import { isAllowedBackfillUrl } from '../lib/backfill/tab-port';
import { renderPopup, popupText, coverageLine, NO_FAILURES } from '../lib/popup-view';
import type { Clock } from '../lib/backfill/pace';
import { stateKey } from '../lib/backfill/types';

const CHATGPT_ORIGIN = 'https://chatgpt.com';
const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
const CLAUDE_ORIGIN = 'https://claude.ai';

function fakeClock(): Clock {
  let t = Date.parse('2026-08-17T00:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

function ids(n: number): string[] {
  return Array.from({ length: n }, (_, i) => `conv-${String(i).padStart(4, '0')}-aaaaaaaa`);
}

function listBody(all: string[]): string {
  return JSON.stringify({
    items: all.map((id) => ({ id, title: 'synthetic-fixture' })),
    total: all.length,
    limit: 100,
    offset: 0,
  });
}

/** A synthetic backend. It records every requested URL — "not one request was sent" has to be proven with it. */
function backend(all: string[], listText?: string) {
  const calls: string[] = [];
  const http = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? 0);
      return { status: 200, text: listText ?? listBody(all.slice(offset, offset + 100)) };
    }
    const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
    return {
      status: 200,
      text: JSON.stringify({ current_node: `${id}-n`, mapping: { [`${id}-n`]: {} } }),
    };
  };
  return { http, calls };
}

// ---------------------------------------------------------------------------
// Criterion 1 · a platform that can be filled in: a synthetic response ⇒ N rows enumerated ⇒ into the debt set (N ≥ 2)
// ---------------------------------------------------------------------------
describe('C22-1 · a backfillable platform really does enumerate', () => {
  it('a synthetic list response ⇒ N≥2 rows in the debt set, and no halt', async () => {
    const store = memoryStore();
    const all = ids(5);
    const be = backend(all);

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: CHATGPT_ORIGIN,
      scope: 'acct-c22',
      store,
      http: be.http,
      clock: fakeClock(),
      // Only the enumeration segment runs: not one body is fetched (budget=0); this case only proves "the list reaches the debt set".
      maxDetails: 0,
    });

    expect(report.halted).toBeNull();
    expect(report.enumeratedPages).toBe(1);
    expect(report.newDebts).toBe(5);
    expect(report.newDebts).toBeGreaterThanOrEqual(2);
    expect(report.state.pending).toEqual(all);
    expect(report.state.totalKnown).toBe(5);
    // 🔴 The enumeration segment must use the chatgpt plan's own path.
    expect(be.calls[0]).toBe(`${CHATGPT_ORIGIN}/backend-api/conversations?offset=0&limit=100`);
  });

  it('a plan counts as "backfillable" only with all seven declarations present', () => {
    const plan = backfillPlanFor('chatgpt');
    expect(plan).not.toBeNull();
    expect(plan!.listPath).toBe('/backend-api/conversations');
    expect(plan!.detailPath).toBe('/backend-api/conversation/');
    expect(plan!.listUrl(CHATGPT_ORIGIN, 20, 50))
      .toBe(`${CHATGPT_ORIGIN}/backend-api/conversations?offset=20&limit=50`);
    // Since C26 detailUrl's type is `(...) => string | null` (null = the body segment has no source yet),
    // and ChatGPT's plan has both segments, so this asserts it is not null before calling it.
    expect(plan!.detailUrl).not.toBeNull();
    expect(plan!.detailUrl!(CHATGPT_ORIGIN, 'a b')).toBe(`${CHATGPT_ORIGIN}/backend-api/conversation/a%20b`);
    expect(typeof plan!.parseListPage).toBe('function');
    // The provenance item is **mandatory** too — "there is no external source" has to be written out, not left blank.
    expect(plan!.provenance.length).toBeGreaterThan(0);
  });
});

// ---------------------------------------------------------------------------
// Criterion 2 · 🔴 a shape mismatch ⇒ halt('shape-changed') with a trace, not in silence
// ---------------------------------------------------------------------------
describe('C22-2 · a shape mismatch must leave a trace', () => {
  it('a list response with no items ⇒ halt(shape-changed), persisted into the debt set', async () => {
    const store = memoryStore();
    // The platform renamed items to conversations (a synthetic "API change").
    const drifted = JSON.stringify({ conversations: [{ id: 'x-0000-aaaaaaaa' }], total: 1 });
    const be = backend([], drifted);

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: CHATGPT_ORIGIN,
      scope: 'acct-drift',
      store,
      http: be.http,
      clock: fakeClock(),
    });

    // 🔴 None of the three may be missing: it stopped, the reason is right, and a trace was left.
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('items');
    // 🔴 The opposite of silence: the trace must be **persisted** and still be there after a restart.
    const persisted = await store.load(stateKey('chatgpt', 'acct-drift'));
    expect((persisted as { halted?: { reason: string } }).halted?.reason).toBe('shape-changed');
    // 🔴 It must never be taken as "0 rows enumerated": not one debt was enqueued, but the ledger says why.
    expect(report.newDebts).toBe(0);
    expect(report.state.enumCursor.complete).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Criterion 3 · 🔴 an unsupported platform = an explicit "not supported", not "0 rows enumerated"
// ---------------------------------------------------------------------------
describe('C22-3 · "we cannot read your history yet" and "you have no history" must stay separate', () => {
  // 🔴 C26 changed this case's **protagonist** (deepseek → claude); the criterion did not change a character.
  //    Reason: C26 filled in DeepSeek's conversation-list cell (a multi-source provenance, see DEEPSEEK_PLAN),
  //    so it is no longer "a platform not supported yet" and using it as the protagonist would no longer exercise this criterion.
  //    claude still stands at "the organization number in the list endpoint's address cannot be obtained", which makes it the right protagonist now.
  //    DeepSeek's own new outcome (the list can be listed, the body segment has no source ⇒ halt('detail-unsupported'))
  //    is watched separately in tests/c27-pplx.test.ts — 🔴 W8 moved that outcome's protagonist from
  //    deepseek to perplexity, for the same reason C26 moved this one, and the criterion is unchanged.
  // 🔴 W31 (2026-09-14) changed this case's **protagonist** for the third time (deepseek → claude →
  //    "a platform whose plan lookup answers null"), and this time the reason is structural rather
  //    than a roster move: claude was the last row in BACKFILL_UNSUPPORTED, its three gaps were
  //    closed by the W20 research, and the table is now empty — so **no platform in the platform
  //    table is undeclared any more**, and there is nothing left that reaching for the real table
  //    could exercise.
  //    What is exercised here is the mechanism, and it is exercised from the same place it always
  //    was: the plan lookup the engine is handed. `plans: () => null` is "this platform's plan is
  //    absent", which is precisely what BACKFILL_UNSUPPORTED used to say about a real row — the
  //    criterion (a platform with no plan halts by name, before any request, and the trace says
  //    what is missing) is unchanged, and the two assertions this test always made are still made
  //    against the very branch that used to serve claude.
  it('a platform with no plan ⇒ halt(unsupported-platform), and not one request was sent', async () => {
    const store = memoryStore();
    const be = backend([]);

    const report = await runBackfill({
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope: 'acct-cl',
      store,
      http: be.http,
      clock: fakeClock(),
      plans: () => null,
    });

    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('unsupported-platform');
    // 🔴 It stops **before** issuing any request — this used to fire ChatGPT's path at DeepSeek.
    expect(be.calls).toEqual([]);
    // The trace must name what is missing, not just say "not supported". With the table empty,
    // the branch that fires is the one that says a row is registered in neither table, and it names
    // both of them — which is the same promise: the trace points at where the gap has to be closed.
    expect(report.halted?.detail).toContain('registered in neither');
    expect(report.halted?.detail).toContain('BACKFILL_UNSUPPORTED');
  });

  it('the control: a supported platform whose list really is empty ⇒ not a halt, but "finished, no history"', async () => {
    const store = memoryStore();
    const be = backend([]);

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: CHATGPT_ORIGIN,
      scope: 'acct-empty',
      store,
      http: be.http,
      clock: fakeClock(),
    });

    // 🔴 Contrasting with the case above: the two outcomes look completely different in the ledger.
    expect(report.stopped).toBe('queue-empty');
    expect(report.halted).toBeNull();
    expect(report.newDebts).toBe(0);
    expect(report.state.enumCursor.complete).toBe(true);
    expect(be.calls.length).toBe(1);
  });

  it('the content script sends no request on behalf of a platform that "cannot be backfilled yet"', () => {
    // A supported platform: its own two paths are allowed.
    expect(isAllowedBackfillUrl(`${CHATGPT_ORIGIN}/backend-api/conversations?offset=0&limit=100`, CHATGPT_ORIGIN)).toBe(true);
    expect(isAllowedBackfillUrl(`${CHATGPT_ORIGIN}/backend-api/conversation/abc`, CHATGPT_ORIGIN)).toBe(true);
    // 🔴 A platform not supported yet: even a lookalike path is refused outright.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/backend-api/conversations`, DEEPSEEK_ORIGIN)).toBe(false);
    // 🔴 C26 · this one went from false to true; what changed is the **fact**, not the criterion.
    //    Under C22 DeepSeek was in BACKFILL_UNSUPPORTED (no plan), so check 3, "this platform can
    //    really be backfilled", rejected every one of its URLs.
    //    C26 filled in its conversation-list cell (R25's multi-source provenance ⇒ DEEPSEEK_PLAN),
    //    so its listPath is now the path the plan **wrote down itself**, and the allowlist lets it through as-is.
    //    🔴 The mechanism that lets it through is unchanged: it is still checkBackfillRequest's four
    //    checks (same origin + platform table + has a plan + path equality), with no bypass and no
    //    loosening into a prefix wildcard —
    //    the two counter-examples below are the evidence for that sentence.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/fetch_page`, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 W8 · **This line went from false to true, and the fact is what changed, not the criterion.**
    //    Under C26 DeepSeek's body segment was null (no source for a single conversation's route), so
    //    checkBackfillRequest's path rule let no body URL through. W8 filled that segment in from two
    //    independent kinds of evidence (a real logged-in browser session on 2026-09-13, plus several
    //    independent implementations), so `chat/history_messages` is now a path the plan **wrote down
    //    itself** — which is the only thing this check has ever asked for. The mechanism is unchanged:
    //    same origin + in the platform table + has a plan + the plan's own path + (W8) the plan's own
    //    query. The two counter-examples below still pin that it did not become a prefix wildcard.
    //    🔴 The half-leg case this line used to demonstrate now belongs to Perplexity — tests/c27-pplx.test.ts.
    expect(isAllowedBackfillUrl(
      `${DEEPSEEK_ORIGIN}/api/v0/chat/history_messages?chat_session_id=x`, DEEPSEEK_ORIGIN)).toBe(true);
    // 🔴 And the tightening W8 added, in the same breath: that path with no declared query is refused.
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat/history_messages`, DEEPSEEK_ORIGIN)).toBe(false);
    // A path that looks like a prefix but is not equal byte for byte is still refused (proving it did not become a prefix wildcard).
    expect(isAllowedBackfillUrl(`${DEEPSEEK_ORIGIN}/api/v0/chat_session/fetch_page/extra`, DEEPSEEK_ORIGIN)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 4 · The two tables must **cover** the platform table completely — a new platform that forgot to declare a side goes red here
// ---------------------------------------------------------------------------
describe('C22-4 · every platform must have a definite conclusion', () => {
  it('every row of the platform table lands on exactly one side — backfillable, or registered as temporarily impossible', () => {
    for (const row of PLATFORMS) {
      const plan = backfillPlanFor(row.id);
      const gap = unsupportedBackfillFor(row.id);
      expect(
        (plan === null) !== (gap === null),
        `platform ${row.id} must appear in exactly one of the two tables`,
      ).toBe(true);
    }
    expect(BACKFILL_SUPPORTED_PLATFORMS.length + BACKFILL_UNSUPPORTED_PLATFORMS.length)
      .toBe(PLATFORMS.length);
    // Today's real state, written into the test: 7 platforms, and 4 can backfill history.
    // 🔴 W8 moved deepseek from the unsupported side to the supported one (its body segment was
    //    filled in). The criterion above is untouched; only the row's side changed.
    // 🔴 W21 (2026-09-14) added the grok row to the platform table with a complete plan (list +
    //    a two-step body), so it joins the supported side. Again: the row's side changed, not the
    //    criterion — the first assertion in this test is what makes a half-declared platform red.
    // 🔴 W22 (2026-09-14) moved kimi across for the same kind of reason as W8: the previously
    //    unknown list parameters and response fields were measured in a logged-in session, so its
    //    entry in BACKFILL_UNSUPPORTED was removed **by evidence**. Nothing about the criterion
    //    above moved, and the platform is on the supported side only because it now has both
    //    segments — the note is here so the next reader knows which change moved it, and why the
    //    order is not the platform table's order (kimi sits before grok here because that is where
    //    the table puts it).
    // 🔴 W29 (2026-09-14) moved gemini across the same way and for the same reason: the
    //    2026-09-14 probe measured both rpcids and both response payloads, the W20 research
    //    recorded the request's content type, and the one declaration that was missing —
    //    "the channel's Content-Type closed set holds only application/json" — was widened with
    //    that evidence rather than by relaxing anything.
    // 🔴 W31 (2026-09-14) moved the **last** row across and left BACKFILL_UNSUPPORTED **empty**,
    //    which is a state worth naming rather than letting a reader discover it from an empty
    //    array: claude's three recorded gaps were closed by the W20 research (the organization
    //    resolver, the limit/offset paging parameters, and the list response's array of `uuid`
    //    summaries). So every row of the platform table now has a plan, and the only platform not
    //    on the supported side is Perplexity — which is *half*-declared, not undeclared, and
    //    appears below through `BACKFILL_UNSUPPORTED_PLATFORMS` because that list means "cannot
    //    backfill history in full", not "has no plan".
    //    The `unsupported-platform` halt path and the BACKFILL_UNSUPPORTED table are therefore
    //    unreachable from the real platform table today. They are **kept**, and C22-3 below now
    //    exercises them with a plan lookup that returns null, so the mechanism stays covered for
    //    the next platform that needs it. Nothing about the criterion moved.
    expect(BACKFILL_SUPPORTED_PLATFORMS).toEqual(['deepseek', 'chatgpt', 'gemini', 'claude', 'kimi', 'grok']);
    expect(BACKFILL_UNSUPPORTED_PLATFORMS).toEqual(['perplexity']);
  });

  it('every "temporarily impossible" must name what is missing plus give the user one plain sentence', () => {
    for (const gap of BACKFILL_UNSUPPORTED) {
      expect(gap.missing.length, `${gap.platform} must say what is missing`).toBeGreaterThan(0);
      expect(t(gap.userNoteKey).length).toBeGreaterThan(0);
      // 🔴 It must not hint that the platform is being backfilled right now.
      expect(t(gap.userNoteKey)).not.toMatch(/backfilling now|is backfilling|in progress/);
    }
  });
});

// ---------------------------------------------------------------------------
// 5 · The popup has to say this out loud
// ---------------------------------------------------------------------------
describe('C22-5 · the popup\'s honest explanation', () => {
  it('that line says all three at once: who can backfill, who cannot, and that cannot ≠ broken', () => {
    const line = coverageLine();
    expect(line).toContain('chatgpt');
    for (const id of BACKFILL_UNSUPPORTED_PLATFORMS) expect(line).toContain(id);
    expect(line).toContain('cannot yet');
    expect(line).toContain('not a breakdown');
    // Carrying C18's red line forward: no percent sign and no time estimate in progress-type wording.
    expect(line).not.toContain('%');
    expect(line).not.toContain('estimated');
  });

  it('the line appears in popupText, and the list is not hand-written', () => {
    const out = popupText(renderPopup({
      enabled: true, block: 'no-http-port', state: null, target: null,
      failures: NO_FAILURES,
    }));
    expect(out).toContain(coverageLine());
    expect(out).not.toContain('%');
    // The per-platform sticking point has to be visible too (in the notes).
    // 🔴 W8 changed this assertion's **protagonist**: the "can list conversations, cannot fetch
    //    bodies yet" sentence no longer describes DeepSeek (its body segment was filled in), so the
    //    note is checked on Perplexity, which is where that state now lives. The criterion — a
    //    half-leg platform's sticking point must be readable by the user — is unchanged.
    expect(out).toContain('Perplexity: past conversation bodies cannot be backfilled yet');
    // 🔴 W29 changed this assertion's **protagonist** for the second time, and the same way W8
    //    changed it: gemini is no longer an unsupported platform (its plan was filled in from the
    //    2026-09-14 probe and the W20 research), so the note that has to be readable is claude's —
    //    the last platform with no plan at all. The criterion is unchanged: a platform whose
    //    history cannot be backfilled yet has to say so where the user can read it.
    // 🔴 W31 (2026-09-14) · claude's plan was filled in from the W20 research too, so
    //    BACKFILL_UNSUPPORTED is now empty and there is no such platform left to point at. The
    //    assertion is turned around rather than deleted, and it is the same fact read the other
    //    way: a platform that **has** a plan must not be described as one that cannot be
    //    backfilled. The catalog still holds claude's old sentence (nothing consumes it), so this
    //    line fails the moment some future code starts rendering a note for a supported platform.
    expect(out).not.toContain('Claude: history cannot be backfilled yet');
  });

  it('with halted=unsupported-platform it says "not implemented yet", not "the platform changed"', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: {
        // 🔴 W18 · A header, not a whole state: the debt ids moved to IndexedDB,
        //    so what the popup picks out of storage carries counts.
        v: 2, platform: 'deepseek', scope: 'acct-ds', totalKnown: null, totalSource: 'unknown',
        enumCursor: { offset: 0, complete: false }, pendingCount: 0, archivedCount: 0,
        detailToday: { day: '', count: 0 },
        halted: { reason: 'unsupported-platform', at: 0, detail: 'missing: listUrl' },
      },
      target: { platform: 'deepseek', scope: 'acct-ds' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);
    expect(out).toContain('has no history backfill implemented');
    expect(out).toContain('This is not a platform change');
  });
});
