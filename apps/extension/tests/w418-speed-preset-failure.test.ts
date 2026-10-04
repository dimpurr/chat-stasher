// @vitest-environment jsdom
/**
 * W418a · a silent preset write failure stays visible on the coverage page.
 *
 * The store below is entirely synthetic. It starts at `standard`, accepts a
 * save call without changing its value, and lets the real coverage page paint
 * the read-back into its radio control.
 */

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { SPEED_PRESET_KEY } from '../lib/backfill/speed';

vi.mock('../lib/i18n', () => ({
  initUiLocale: async () => {},
  t: (key: string, values?: Record<string, string | number>) =>
    key === 'coverage.preset.failed' ? `Preset failed: ${values?.detail ?? ''}` : key,
}));

const COVERAGE_HTML = readFileSync(
  resolve(__dirname, '..', 'entrypoints', 'coverage', 'index.html'),
  'utf8',
);

function mountCoverage(): void {
  const start = COVERAGE_HTML.indexOf('<body>') + '<body>'.length;
  const body = COVERAGE_HTML.slice(start, COVERAGE_HTML.lastIndexOf('</body>'));
  document.body.innerHTML = body.replace(/<script[\s\S]*?<\/script>/g, '');
}

function syntheticStorage() {
  const data: Record<string, unknown> = { [SPEED_PRESET_KEY]: 'standard' };
  return {
    async get(query: Record<string, unknown> | null) {
      if (query === null) return { ...data };
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(query)) out[key] = key in data ? data[key] : query[key];
      return out;
    },
    async set(_values: Record<string, unknown>) {
      // Synthetic silent write failure: accept the request, preserve `standard`.
    },
    async remove(keys: string | string[]) {
      for (const key of ([] as string[]).concat(keys)) delete data[key];
    },
  };
}

async function until(ready: () => boolean): Promise<void> {
  for (let i = 0; i < 100; i += 1) {
    if (ready()) return;
    await new Promise((r) => setTimeout(r, 0));
  }
  throw new Error('coverage page did not finish painting');
}

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  mountCoverage();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('W418a · coverage page preset write read-back', () => {
  it('shows failure and restores the stored radio after a silent mismatched write', async () => {
    const browser = {
      runtime: {
        id: 'w418-speed-preset-failure-test',
        async sendMessage() { return null; },
      },
      storage: { local: syntheticStorage() },
    };
    vi.stubGlobal('browser', browser);
    vi.stubGlobal('chrome', browser);

    await import('../entrypoints/coverage/main.ts');
    const error = document.getElementById('error')!;
    await until(() => document.querySelector<HTMLInputElement>('#speed-standard') !== null);
    expect(document.querySelector<HTMLInputElement>('#speed-standard')!.checked).toBe(true);
    expect(error.hidden).toBe(true);

    document.querySelector<HTMLInputElement>('#speed-faster')!.click();
    await until(() => !error.hidden);

    expect(error.textContent).toContain('speed preset write did not persist');
    expect(document.querySelector<HTMLInputElement>('#speed-standard')!.checked).toBe(true);
    expect(document.querySelector<HTMLInputElement>('#speed-faster')!.checked).toBe(false);
  });
});
