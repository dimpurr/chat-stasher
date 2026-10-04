/** W463 · A malformed skipped tick row cannot hide a valid platform reason. */

import { describe, expect, it, vi } from 'vitest';
import { isReadableHeaderAt, readCoverageInputs } from '../lib/coverage-read';
import { BACKFILL_LAST_TICK_KEY } from '../lib/backfill/alarm';
import { memoryStore } from '../lib/backfill/store';
import { headerOf, initialState, stateKey } from '../lib/backfill/types';
import { withI18n } from './i18n-harness';

describe('W463 · malformed tick skip rows', () => {
  it('ignores malformed rows while preserving a valid matching skip reason', async () => {
    const platform = 'deepseek';
    const scope = 'default';
    const state = initialState(platform, scope);
    const header = headerOf(state);
    const tick = {
      at: 1,
      ran: true,
      reason: 'ran',
      targets: 1,
      schedule: {
        skipped: [null, 'malformed', 17, { platform, reason: 'rate-limited' }],
      },
    };
    const localData = { [stateKey(platform, scope)]: header };
    const localArea = {
      async get(query: null | Record<string, unknown>) {
        if (query === null) return { ...localData };
        return Object.fromEntries(Object.keys(query).map((key) => [key, localData[key] ?? null]));
      },
      async set(values: Record<string, unknown>) { Object.assign(localData, values); },
      async remove(keys: string | string[]) {
        for (const key of typeof keys === 'string' ? [keys] : keys) delete localData[key];
      },
    };
    vi.stubGlobal('browser', withI18n({ runtime: { id: 'synthetic-extension-id' }, storage: { local: localArea } } as never));
    expect(isReadableHeaderAt(stateKey(platform, scope), header)).toBe(true);

    const inputs = await readCoverageInputs(memoryStore({ [BACKFILL_LAST_TICK_KEY]: tick }), 10);

    expect(inputs.scopes).toHaveLength(1);
    expect(inputs.scopes[0]?.platform).toBe(platform);
    expect(inputs.scopes[0]?.skippedReason).toBe('rate-limited');
  });
});
