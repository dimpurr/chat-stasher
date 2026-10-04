/** W471 · A damaged length-prefixed frame does not hide a later Gemini RPC. */

import { describe, expect, it } from 'vitest';
import {
  GEMINI_RPC_DETAIL,
  GEMINI_RPC_LIST,
  parseBatchExecute,
  readDetailResponse,
  readListResponse,
} from '../lib/gemini-rpc';

function frame(value: unknown): string {
  const chunk = JSON.stringify(value);
  return `${chunk.length}\n${chunk}\n`;
}

function rpcFrame(rpcid: string, payload: unknown): string {
  return frame([['wrb.fr', rpcid, JSON.stringify(payload), null, null, null, 'generic']]);
}

function malformedFrame(text: string): string {
  return `40\n${text}\n`;
}

function response(...frames: string[]): string {
  return `)]}'\n${frames.join('')}`;
}

describe('Gemini batchexecute frame recovery', () => {
  it('counts broken frames and keeps later list and detail RPCs in order', () => {
    const body = response(
      malformedFrame('[{"broken":'),
      malformedFrame('["also broken"'),
      rpcFrame(GEMINI_RPC_LIST, [null, null, [['c_synthetic1', 'synthetic title']]]),
      rpcFrame(GEMINI_RPC_DETAIL, [[[[ 'c_synthetic1', 'r_synthetic1' ], null, null, null, [1_700_000_000, 0]]], null]),
    );

    const parsed = parseBatchExecute(body);
    expect(parsed.ok).toBe(true);
    if (!parsed.ok) return;

    expect(parsed.undecodedFrameCount).toBe(2);
    expect(parsed.entries.map((entry) => entry.rpcid)).toEqual([GEMINI_RPC_LIST, GEMINI_RPC_DETAIL]);

    const list = readListResponse(body);
    expect(list.ok).toBe(true);
    if (!list.ok) return;
    expect(list.ids).toEqual(['c_synthetic1']);
    expect(list.pageIsEmpty).toBe(false);

    const detail = readDetailResponse(body, 'c_synthetic1');
    expect(detail.ok).toBe(true);
    if (!detail.ok) return;
    expect(detail.responseIds).toEqual(['r_synthetic1']);
    expect(detail.pageIsEmpty).toBe(false);
  });

  it('does not interpret undecodable frames alone as empty list or detail pages', () => {
    const body = response(malformedFrame('[{"broken":'), malformedFrame('["still broken"'));

    expect(readListResponse(body)).toEqual({ ok: false, reason: 'frames-undecodable' });
    expect(readDetailResponse(body, 'c_synthetic1')).toEqual({
      ok: false,
      reason: 'frames-undecodable',
    });
  });
});
