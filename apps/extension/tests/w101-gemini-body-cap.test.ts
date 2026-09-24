/**
 * W101 · Gemini's list continuation body vs the page bridge's byte ceiling.
 *
 * ## The defect this file pins
 * Gemini's list cursor is the platform's own opaque continuation token, and its
 * size grows with the account (measured: 441 chars after 25 items in pinguarmy's
 * real-sanitized fixture; **3993 chars at 280 conversations** in the live ledger).
 * Our builder echoes it once, and a 3993-char cursor produces a **4323-byte**
 * `f.req=…&at=` body — 227 bytes over the JSON ceiling `MAX_REQUEST_BODY_BYTES`
 * (4096). The page bridge refused exactly that request
 * (`list token=set (enumerated 280): refused: request body exceeds
 * MAX_REQUEST_BODY_BYTES`), so the list could never finish.
 *
 * The fix is a ceiling of the form kind's own (`MAX_FORM_REQUEST_BODY_BYTES`),
 * not a raise of the JSON one. These tests assert both halves:
 *  · the measured-size Gemini cursor is admitted (this **fails on main**);
 *  · the form ceiling still refuses a body above it, and the JSON ceiling is
 *    untouched at 4096;
 *  · the refusal stays a **transient** stop, so the platform is never parked.
 *
 * 🔴 All fixtures are synthetic. No request is sent to gemini.google.com; there
 * is no logged-in state and no real token, id or conversation appears here. The
 * synthetic cursor has the measured *length* and base64 alphabet, nothing more.
 */

import { describe, expect, it } from 'vitest';
import {
  GEMINI_LIST_PAGE_SIZE,
  GEMINI_PLAN,
  MAX_FORM_REQUEST_BODY_BYTES,
  MAX_REQUEST_BODY_BYTES,
  listTokenPostInit,
  type BackfillEnumPlan,
} from '../lib/backfill/enumerate';
import { checkBackfillRequest } from '../lib/backfill/tab-port';
import { haltClassOf } from '../lib/backfill/types';
import { GEMINI_FORM_CONTENT_TYPE } from '../lib/platform-auth';
import { runBackfill, type HttpPort } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import type { BackfillPace, Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://gemini.google.com';
const FORM = { method: 'POST', contentType: GEMINI_FORM_CONTENT_TYPE } as const;

/** A synthetic opaque cursor: the measured length, over the real base64 alphabet. */
function syntheticCursor(chars: number): string {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  let out = '';
  for (let i = 0; i < chars; i += 1) out += alphabet[i % alphabet.length];
  return out;
}

function bodyBytes(body: string): number {
  return new TextEncoder().encode(body).byteLength;
}

function listRequest(token: string) {
  const init = listTokenPostInit(GEMINI_PLAN, ORIGIN, token, GEMINI_LIST_PAGE_SIZE);
  return {
    url: GEMINI_PLAN.listUrl(ORIGIN, 0, GEMINI_LIST_PAGE_SIZE),
    ...FORM,
    body: init.body,
  };
}

const NO_WAIT: BackfillPace = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock {
  let time = Date.parse('2026-09-24T00:00:00.000Z');
  return { now: () => time, async sleep(ms: number) { time += ms; } };
}

describe('W101-1 · the Gemini list body and the form-body ceiling', () => {
  it('the measured-size cursor makes a body over 4 KiB, and the form ceiling admits it', () => {
    const request = listRequest(syntheticCursor(3993));
    // The premise: this body is over the JSON ceiling. (The live ledger's token was
    // 3993 chars and produced 4323 bytes; this synthetic one is 4357 bytes — same
    // length and alphabet, so the same order of magnitude and the same defect.)
    expect(bodyBytes(request.body!)).toBeGreaterThan(MAX_REQUEST_BODY_BYTES);
    // 🔴 Red on main, for the behavioural reason and not a missing export: before
    //    W101 the verdict is `false` (refused at 4096), and Gemini's list can never
    //    read page 15 of a 280-conversation account.
    const verdict = checkBackfillRequest(request, ORIGIN);
    expect(verdict.ok).toBe(true);
  });

  it('the form ceiling still exists: a body above it is refused by name', () => {
    const request = listRequest(syntheticCursor(MAX_FORM_REQUEST_BODY_BYTES + 64));
    expect(bodyBytes(request.body!)).toBeGreaterThan(MAX_FORM_REQUEST_BODY_BYTES);
    const verdict = checkBackfillRequest(request, ORIGIN);
    expect(verdict.ok).toBe(false);
    if (verdict.ok) return;
    expect(verdict.detail).toContain('MAX_FORM_REQUEST_BODY_BYTES');
  });

  it('the JSON ceiling did not move: 4096 is still the limit for a JSON body', () => {
    expect(MAX_REQUEST_BODY_BYTES).toBe(4096);
    expect(MAX_FORM_REQUEST_BODY_BYTES).toBeGreaterThan(MAX_REQUEST_BODY_BYTES);
    // A JSON kind (injected plan, the same seam C23 uses) over 4096 bytes is still refused.
    const jsonPlan: BackfillEnumPlan = {
      platform: 'chatgpt',
      provenance: 'synthetic · W101 test plan, injected through checkBackfillRequest’s lookup seam',
      listPath: '/backend-api/conversations',
      listUrl: (origin, offset, limit) => `${origin}/backend-api/conversations?offset=${offset}&limit=${limit}`,
      parseListPage: () => ({ ok: true, page: { ids: [], total: null, nextToken: null } }),
      detailPath: null,
      detailUrl: null,
      listPost: {
        contentType: 'application/json',
        bodyKeys: ['offset', 'limit'],
        body: (_origin, offset, limit) => JSON.stringify({ offset, limit }),
      },
    };
    const verdict = checkBackfillRequest(
      {
        url: 'https://chatgpt.com/backend-api/conversations',
        method: 'POST',
        body: JSON.stringify({ offset: 'x'.repeat(MAX_REQUEST_BODY_BYTES), limit: 1 }),
        contentType: 'application/json',
      },
      'https://chatgpt.com',
      () => jsonPlan,
    );
    expect(verdict.ok).toBe(false);
  });
});

describe('W101-2 · a list refusal never parks the platform', () => {
  it('the body-cap refusal is a transient stop with a retry moment, not a permanent one', async () => {
    const store = memoryStore();
    const http: HttpPort = async () => {
      throw new Error('refused: request body exceeds MAX_FORM_REQUEST_BODY_BYTES');
    };
    const report = await runBackfill({
      platform: 'gemini',
      origin: ORIGIN,
      scope: 'acct-w101',
      store,
      http,
      clock: fakeClock(),
      pace: NO_WAIT,
      sink: () => ({ saved: true, sessionId: 'synthetic' }),
    });
    expect(report.halted?.reason).toBe('transport-error');
    // 🔴 "Transient" is the property under test: a permanent record is never cleared
    //    by the product, so a list refusal classified permanent would freeze Gemini
    //    until a human edited storage.
    expect(haltClassOf(report.halted!.reason)).toBe('transient');
    expect(typeof report.state.halted?.retryAt).toBe('number');
    expect(report.newDebts).toBe(0);
  });
});
