import { afterEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { HOOK_REASON_WAS_REPLACED } from '../lib/contract';
import { NO_FAILURES, renderPopup, type PopupModel } from '../lib/popup-view';
import type { HookStatusRecord } from '../lib/hook-status';

const fakeBrowser: any = { runtime: {} };
vi.stubGlobal('browser', withI18n(fakeBrowser));
vi.stubGlobal('chrome', fakeBrowser);

function model(hookStatus: HookStatusRecord[]): PopupModel {
  return {
    enabled: true,
    block: null,
    state: null,
    target: null,
    failures: NO_FAILURES,
    hookStatus,
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('W428 · the popup hook diagnostic is useful and redacted', () => {
  it('names the affected platform and exposes only its origin, reason, and time', () => {
    const at = Date.parse('2026-10-04T10:11:12.000Z');
    const path = '/c/synthetic-conversation-id';
    const query = '?token=synthetic-secret-token';
    const body = 'synthetic private conversation body';
    const record: HookStatusRecord = {
      origin: `https://chatgpt.com${path}${query}#${body}`,
      platform: 'chatgpt',
      reasons: [{ reason: HOOK_REASON_WAS_REPLACED, at }],
      at,
    };

    const diagnostic = renderPopup(model([record])).notes.join('\n');

    expect(diagnostic).toContain('ChatGPT');
    expect(diagnostic).toContain('https://chatgpt.com');
    expect(diagnostic).toContain('the hook was installed, and the page took that global back afterwards');
    expect(diagnostic).toContain('2026-10-04 10:11:12 UTC');
    expect(diagnostic).not.toContain(path);
    expect(diagnostic).not.toContain('synthetic-conversation-id');
    expect(diagnostic).not.toContain('synthetic-secret-token');
    expect(diagnostic).not.toContain(body);
    expect(diagnostic).not.toContain('token=');
  });
});
