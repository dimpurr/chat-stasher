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
 * 🔴 A `lastKnownStage` on the record is the only thing that may turn the fix
 *    into a `chat-stasher …` command (topology principle 8 — see fixCommand in
 *    lib/ui-strings.ts), so both halves of that decision are pinned here: with a
 *    stage ever learned ⇒ the CLI it installed is named, and with none ⇒ the
 *    one-line installer, never a command that assumes a CLI nothing has shown
 *    to exist on this machine. Pinned on both sides because each is only
 *    meaningful against the other.
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

  // -------------------------------------------------------------------------
  // 🔴 The other half of the fix decision: a stage *was* learned, so the CLI
  //    that installed it is named. `lastKnownStage` survives failures
  //    (lib/host-status.ts), which is exactly what makes it evidence — and it
  //    is labelled "last known" here, because a stage from an earlier success
  //    is not a current fact. This is the only way out of the installer.
  // -------------------------------------------------------------------------

  it('channelDisconnected with a lastKnownStage — the learned-stage sentence and the CLI command', async () => {
    const ui = await uiStrings();
    expect(ui.channelDisconnected({
      at: 1700000000000,
      ok: false,
      reason: 'timeout',
      lastKnownStage: '/Users/me/stage',
    })).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: timeout (checked 2023-11-14 22:13:20 UTC). Last known stage: /Users/me/stage. Fix: chat-stasher install-native-host --stage /Users/me/stage',
    );
  });

  // 🔴 `channelDisconnected` reads `lastKnownStage` and never `stage`. A stage
  //    the *failed* check reported is not evidence the CLI exists, and reading
  //    it as such would name a `chat-stasher …` command on a machine where
  //    nothing has ever proved one. Pinned because the two fields are
  //    different claims about different moments.
  it("a failed check's own stage field does not licence the CLI command", async () => {
    const ui = await uiStrings();
    const line = ui.channelDisconnected({
      at: 1700000000000,
      ok: false,
      reason: 'stage-unavailable',
      stage: '/Users/me/stage',
    });
    expect(line).toContain('No stage path has ever been learned on this machine.');
    expect(line).toContain('curl -fsSL https://chatstasher.com/install.sh | sh');
    expect(line).not.toContain('chat-stasher install-native-host');
  });

  // -------------------------------------------------------------------------
  // The two reasons the sentence can carry. `reason` alone is the plain case;
  // a host `nack` also carries the `kind`, which is a narrower code — both go
  // in the sentence rather than one replacing the other.
  // -------------------------------------------------------------------------

  it('a nack carries its kind beside the reason, not instead of it', async () => {
    const ui = await uiStrings();
    expect(ui.channelDisconnected({
      at: 0,
      ok: false,
      reason: 'nack',
      kind: 'protocol-version',
    })).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: nack (protocol-version) (checked 1970-01-01 00:00:00 UTC). No stage path has ever been learned on this machine. Fix: curl -fsSL https://chatstasher.com/install.sh | sh',
    );
  });

  /**
   * 🔴 The host named no reason at all. "We do not know why" is its own
   *    sentence, not an empty one and not a reason borrowed from somewhere
   *    else — the same three-states rule the rest of the tool keeps.
   */
  it('no reason at all reads "unknown reason", never a blank and never a guess', async () => {
    const ui = await uiStrings();
    expect(ui.channelDisconnected({ at: 0, ok: false })).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: unknown reason (checked 1970-01-01 00:00:00 UTC). No stage path has ever been learned on this machine. Fix: curl -fsSL https://chatstasher.com/install.sh | sh',
    );
  });

  // The `detail` a nack carries is appended between the headline and the stage,
  // so it reads as part of the reason rather than as the whole of it.
  it('a detail is appended to the headline, and never replaces it', async () => {
    const ui = await uiStrings();
    const line = ui.channelDisconnected({
      at: 1700000000000,
      ok: false,
      reason: 'nack',
      kind: 'config',
      detail: 'no stage configured',
      lastKnownStage: '/Users/me/stage',
    });
    expect(line).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: nack (config) (checked 2023-11-14 22:13:20 UTC). Detail: no stage configured. Last known stage: /Users/me/stage. Fix: chat-stasher install-native-host --stage /Users/me/stage',
    );
    // The detail is additive: the reason and the kind are both still readable.
    expect(line).toContain('reason: nack (config)');
    expect(line).toContain('Detail: no stage configured.');
  });

  /**
   * 🔴 An empty `detail` is no detail. The sentence must not carry a
   *    `Detail: .` with nothing in it — an unknown must never be rendered as
   *    empty (CLAUDE.md invariant 1).
   */
  it('an empty detail contributes no sentence at all', async () => {
    const ui = await uiStrings();
    const line = ui.channelDisconnected({
      at: 1700000000000,
      ok: false,
      reason: 'timeout',
      detail: '',
    });
    expect(line).not.toContain('Detail:');
    expect(line).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: timeout (checked 2023-11-14 22:13:20 UTC). No stage path has ever been learned on this machine. Fix: curl -fsSL https://chatstasher.com/install.sh | sh',
    );
  });

  // -------------------------------------------------------------------------
  // 🔴 A fact we were never given must read as "unknown", never as the
  //    JavaScript `undefined`. `parseHostStatus` only shape-validates the fields
  //    it knows, so a record written by an older build — or a hand-edited one —
  //    reaches this builder with them absent, and interpolating the absence
  //    shipped the literal text "stage undefined" to a user. Pinned on all three
  //    facts and on the string itself, because the failure is a word appearing
  //    that no catalog entry contains.
  // -------------------------------------------------------------------------

  it('a connected record missing its facts reads "unknown", never the string "undefined"', async () => {
    const ui = await uiStrings();
    const line = ui.channelConnected({ at: 1700000000000, ok: true });
    expect(line).toBe(
      'Delivery channel: connected to the chat-stasher host — stage unknown · machine unknown · host version unknown (checked 2023-11-14 22:13:20 UTC).',
    );
    expect(line).not.toContain('undefined');
  });

  it('one absent connected fact does not blank the two it does have', async () => {
    const ui = await uiStrings();
    const line = ui.channelConnected({ at: 1700000000000, ok: true, stage: '/Users/me/stage', hostVersion: '0.3.0' });
    expect(line).toBe(
      'Delivery channel: connected to the chat-stasher host — stage /Users/me/stage · machine unknown · host version 0.3.0 (checked 2023-11-14 22:13:20 UTC).',
    );
  });

  // 🔴 `reason` and `kind` are independent fields. A kind present does not make
  //    the reason known, and reading it that way rendered "reason: undefined
  //    (config)" — the same missing-fact-as-a-value defect, in the sentence that
  //    tells a user why their host is not answering.
  it('a kind with no reason reads "unknown reason" beside the kind', async () => {
    const ui = await uiStrings();
    const line = ui.channelDisconnected({ at: 1700000000000, ok: false, kind: 'config' });
    expect(line).toBe(
      'Delivery channel: NOT connected to the chat-stasher host — reason: unknown reason (config) (checked 2023-11-14 22:13:20 UTC). No stage path has ever been learned on this machine. Fix: curl -fsSL https://chatstasher.com/install.sh | sh',
    );
    expect(line).not.toContain('undefined');
  });
});
