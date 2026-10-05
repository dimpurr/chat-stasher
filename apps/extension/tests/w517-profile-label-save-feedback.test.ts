// @vitest-environment jsdom
/** W517 · profile-label save feedback on the real popup path. */

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { POPUP_SAVE_INSTALL_LABEL_MESSAGE, POPUP_STATUS_MESSAGE } from '../lib/popup-view';
import { withI18n } from './i18n-harness';
import { createSyntheticHost } from './synthetic-native-host';

const POPUP_HTML = readFileSync(
  resolve(__dirname, '..', 'entrypoints', 'popup', 'index.html'),
  'utf8',
);

function mountPopup(): void {
  const start = POPUP_HTML.indexOf('<body>') + '<body>'.length;
  const body = POPUP_HTML.slice(start, POPUP_HTML.lastIndexOf('</body>'));
  document.body.innerHTML = body.replace(/<script[\s\S]*?<\/script>/g, '');
}

function memoryArea() {
  const data: Record<string, unknown> = {};
  return {
    async get(query: Record<string, unknown> | null) {
      if (query === null) return { ...data };
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(query)) out[key] = key in data ? data[key] : query[key];
      return out;
    },
    async set(values: Record<string, unknown>) { Object.assign(data, values); },
    async remove(keys: string | string[]) {
      for (const key of ([] as string[]).concat(keys)) delete data[key];
    },
  };
}

async function until(ready: () => boolean, what: string): Promise<void> {
  for (let i = 0; i < 200; i += 1) {
    if (ready()) return;
    await new Promise((r) => setTimeout(r, 0));
  }
  throw new Error(`timed out waiting for ${what}`);
}

type Outcome = 'ok' | 'refused' | 'throws';

async function boot(outcome: Outcome) {
  mountPopup();
  const host = createSyntheticHost();
  const sent: Array<Record<string, unknown>> = [];
  let savedLabel: string | null = null;
  const fake: any = {
    runtime: {
      id: 'w517-profile-label-save-feedback-test',
      lastError: undefined,
      async sendMessage(message: Record<string, unknown>) {
        sent.push(message);
        if (message.type === POPUP_STATUS_MESSAGE) {
          return {
            transportWired: false,
            lastTickReason: null,
            liveTarget: null,
            install: {
              install_id: 'synthetic-install-id',
              browser: 'Chrome',
              profile_label: savedLabel,
            },
          };
        }
        if (message.type === POPUP_SAVE_INSTALL_LABEL_MESSAGE) {
          if (outcome === 'throws') throw new Error('synthetic exception with private detail');
          if (outcome === 'refused') return { ok: false, detail: 'synthetic host detail' };
          savedLabel = String(message.profile_label);
          return { ok: true };
        }
        return null;
      },
      async sendNativeMessage(name: string, message: unknown) {
        return host.sendNativeMessage(name, message);
      },
    },
    storage: { local: memoryArea() },
    tabs: { async create() { return { id: 1 }; } },
  };
  vi.stubGlobal('browser', withI18n(fake));
  vi.stubGlobal('chrome', fake);
  await import('../entrypoints/popup/main.ts');
  const input = document.getElementById('install-profile-label') as HTMLInputElement;
  const button = document.getElementById('save-install-profile-label') as HTMLButtonElement;
  const status = document.getElementById('install-profile-label-save-status') as HTMLElement;
  await until(() => !document.getElementById('install-name-row')!.hasAttribute('hidden'), 'profile label row');
  return { input, button, status, sent };
}

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.spyOn(console, 'warn').mockImplementation(() => {});
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('W517 · profile-label save feedback', () => {
  it('announces success only after an ok reply and keeps the existing save request', async () => {
    const { input, button, status, sent } = await boot('ok');
    input.value = 'Synthetic profile label';
    button.click();

    await until(() => status.textContent === 'Profile name saved.', 'save success announcement');
    const request = sent.find((message) => message.type === POPUP_SAVE_INSTALL_LABEL_MESSAGE);
    expect(request).toEqual({ type: POPUP_SAVE_INSTALL_LABEL_MESSAGE, profile_label: 'Synthetic profile label' });
  });

  it.each(['refused', 'throws'] as const)('announces a safe retry action when the host %s', async (outcome) => {
    const { input, button, status } = await boot(outcome);
    input.value = 'Synthetic profile label';
    button.click();

    await until(() => status.textContent === 'Could not save the profile name. Try again.', 'safe failure announcement');
    expect(status.textContent).not.toContain('Synthetic profile label');
    expect(status.textContent).not.toContain('synthetic exception');
    expect(status.textContent).not.toContain('synthetic host detail');
  });

  it('exposes the outcome through a polite atomic status region', async () => {
    const { status } = await boot('refused');
    expect(status.getAttribute('role')).toBe('status');
    expect(status.getAttribute('aria-live')).toBe('polite');
    expect(status.getAttribute('aria-atomic')).toBe('true');
  });
});
