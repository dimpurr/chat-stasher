/**
 * W127b · P1 — a 429/503 must keep its `Retry-After` even when the body cannot be read.
 *
 * The defect: `serveBackfillFetch` read and size-checked the body **before** it
 * forwarded the status and the header. If `res.text()` rejected, or the body was
 * over `MAX_RAW_BYTES`, the content script answered `{ok:false}`; `tabHttpPort`
 * turns that into a thrown error, and the engine halts `transport-error` — a
 * shorter, unrequested retry instead of the rate-limit the platform sent.
 *
 * 🔴 Everything here is synthetic: fixture URLs, fixture responses, an injected
 *    clock and build id. No network, no account, no conversation text.
 */

import { describe, it, expect, vi } from 'vitest';

import { MAX_RAW_BYTES } from '../lib/contract';
import {
  runBackfill,
  type HttpPort,
} from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  DEFAULT_ENUM_PACE,
  DEFAULT_DETAIL_PACE,
  type Clock,
} from '../lib/backfill/pace';
import { CHATGPT_PLAN } from '../lib/backfill/enumerate';
import {
  handleBackfillMessage,
  serveBackfillFetch,
  tabHttpPort,
} from '../lib/backfill/tab-port';
import { TEST_BUILD_ID } from './i18n-harness';

const ORIGIN = 'https://chatgpt.com';
const LIST_URL = `${ORIGIN}${CHATGPT_PLAN.listPath}`;
const NO_WAIT = {
  enumerate: { ...DEFAULT_ENUM_PACE, minIntervalMs: 0 },
  detail: { ...DEFAULT_DETAIL_PACE, minIntervalMs: 0 },
};

function fakeClock(): Clock {
  let t = Date.parse('2026-09-25T12:00:00.000Z');
  return { now: () => t, async sleep(ms: number) { t += ms; } };
}

// ===========================================================================
// The content script: the header and status outlive the body
// ===========================================================================

describe('W127b-P1 · serveBackfillFetch forwards a 429/503 status and Retry-After without the body', () => {
  it('🔴 a 429 whose body read throws still carries the status and the header', async () => {
    const fetchImpl = async () => ({
      status: 429,
      text: async (): Promise<string> => { throw new Error('synthetic body read failure'); },
      retryAfter: '120',
    });
    const reply = await serveBackfillFetch(LIST_URL, ORIGIN, fetchImpl as never);
    expect(reply).toEqual({ ok: true, status: 429, text: '', retryAfter: '120' });
  });

  it('🔴 a 503 whose body exceeds MAX_RAW_BYTES still carries the status and the header', async () => {
    const oversized = 'x'.repeat(MAX_RAW_BYTES + 1);
    const fetchImpl = async () => ({
      status: 503,
      text: async () => oversized,
      retryAfter: '45',
    });
    const reply = await serveBackfillFetch(LIST_URL, ORIGIN, fetchImpl as never);
    expect(reply).toEqual({ ok: true, status: 503, text: '', retryAfter: '45' });
  });

  it('🔴 the rescued reply keeps a header that is absent absent, not invented', async () => {
    const fetchImpl = async () => ({
      status: 429,
      text: async (): Promise<string> => { throw new Error('synthetic body read failure'); },
    });
    const reply = await serveBackfillFetch(LIST_URL, ORIGIN, fetchImpl as never);
    expect(reply).toEqual({ ok: true, status: 429, text: '' });
  });

  it('🔴 the normal path is unchanged: a readable 429 body keeps its text and header', async () => {
    const fetchImpl = async () => ({ status: 429, text: async () => 'rate limited', retryAfter: '120' });
    const reply = await serveBackfillFetch(LIST_URL, ORIGIN, fetchImpl as never);
    expect(reply).toEqual({ ok: true, status: 429, text: 'rate limited', retryAfter: '120' });
  });

  it('🔴 a 2xx with an unreadable or oversized body is still a transport failure, not a rescued status', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const throws = async () => ({
      status: 200,
      text: async (): Promise<string> => { throw new Error('synthetic body read failure'); },
    });
    expect(await serveBackfillFetch(LIST_URL, ORIGIN, throws as never))
      .toEqual({ ok: false, error: 'synthetic body read failure' });

    const oversized = async () => ({ status: 200, text: async () => 'x'.repeat(MAX_RAW_BYTES + 1) });
    expect(await serveBackfillFetch(LIST_URL, ORIGIN, oversized as never))
      .toEqual({ ok: false, error: 'refused: response exceeds MAX_RAW_BYTES' });
    warn.mockRestore();
  });
});

// ===========================================================================
// The whole chain: the engine sees the rate-limit, never a transport error
// ===========================================================================

describe('W127b-P1 · through the tab port the engine sees rate-limited, not transport-error', () => {
  it('🔴 a 429 whose body read fails still halts rate-limited and honours the header', async () => {
    const pageFetch = async () => ({
      status: 429,
      text: async (): Promise<string> => { throw new Error('synthetic body read failure'); },
      retryAfter: '120',
    });
    const http: HttpPort = tabHttpPort(1, async (_id, msg) =>
      handleBackfillMessage(msg, ORIGIN, pageFetch as never));

    const clock = fakeClock();
    const report = await runBackfill({
      platform: 'chatgpt',
      origin: ORIGIN,
      scope: 'w127b-p1-chain',
      store: memoryStore(),
      http,
      clock,
      build: TEST_BUILD_ID,
      pace: NO_WAIT,
      random: () => 0,
    });

    expect(report.halted?.reason).toBe('rate-limited');
    expect(report.halted?.retryAt).toBe(report.halted!.at + 120_000);
  });
});
