/**
 * W666 · The two channel-status builders, pinned directly.
 *
 * `channelLine` (lib/popup-view.ts) renders these through the popup model,
 * so their wording has only ever been asserted with `toContain` on the
 * assembled line. These are the builders' own tests: the exact sentence
 * each one builds, for the two timestamps a channel-status record carries —
 * the epoch (a check that ran at the start of time) and a real moment —
 * with the facts each builder interpolates.
 *
 * The strings come out of `locales/en.yml` through the shared harness
 * (tests/i18n-harness.ts), so what is pinned here is what ships. The
 * expected strings are written out in full, timestamp included, so a change
 * to either the wording or the stamp fails here rather than wherever the
 * popup happens to show it.
 *
 * 🔴 The disconnected cases carry no `lastKnownStage`: nothing on this
 *    machine has ever learned a stage, so the fix is the one-line installer
 *    and never a `chat-stasher …` command (topology principle 8 — see
 *    fixCommand in lib/ui-strings.ts). That is half of what these pins
 *    hold.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';

const fakeBrowser = withI18n({ runtime: { id: 'w666-channel-status' } });

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
});

/**
 * The builders, freshly imported against the stubbed browser, with the
 * English catalog loaded through the extension's own locale path — the
 * configuration a user who picked English runs.
 */
async function uiStrings(): Promise<typeof import('../lib/ui-strings')> {
  const overlay = await import('../lib/i18n');
  const strings = await import('../lib/ui-strings');
  await overlay.applyUiLocale('en');
  return strings;
}

/** What a `hello` that succeeded writes down: everything `channelConnected` interpolates. */
const CONNECTED = {
  ok: true,
  kind: 'native',
  stage: '/Users/me/stage',
  machine: 'mac-1',
  hostVersion: '0.3.0',
} as const;

describe('W666 · channelConnected / channelDisconnected', () => {
  it('channelConnected(0, "native") — the catalog sentence, checked at the epoch', async () => {
    const ui = await uiStrings();
    expect(ui.channelConnected({ at: 0, ...CONNECTED })).toBe(
      'Delivery channel: connected to the chat-stasher host — stage /Users/me/stage · machine mac-1 · host version 0.3.0 (checked 1970-01-01 00:00:00 UTC).',
    );
  });

  it('channelConnected(1700000000000, "native") — the same sentence, checked at a real moment', async () => {
    const ui = await uiStrings();
    expect(ui.channelConnected({ at: 1700000000000, ...CONNECTED })).toBe(
      'Delivery channel: connected to the chat-stasher host — stage /Users/me/stage · machine mac-1 · host version 0.3.0 (checked 2023-11-14 22:13:20 UTC).',
    );
  });

  it('channelDisconnected(0, "websocket-closed") — reason, never-learned stage, installer fix', async () => {
    const ui = await uiStrings();
    expect(ui.channelDisconnected({ at: 0, ok: false, reason: 'websocket-closed' })).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: websocket-closed (checked 1970-01-01 00:00:00 UTC). No stage path has ever been learned on this machine. Fix: curl -fsSL https://chatstasher.com/install.sh | sh',
    );
  });

  it('channelDisconnected(1700000000000, "websocket-closed") — the same sentence, checked at a real moment', async () => {
    const ui = await uiStrings();
    expect(ui.channelDisconnected({ at: 1700000000000, ok: false, reason: 'websocket-closed' })).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: websocket-closed (checked 2023-11-14 22:13:20 UTC). No stage path has ever been learned on this machine. Fix: curl -fsSL https://chatstasher.com/install.sh | sh',
    );
  });
});
