/**
 * W214 · EXT-12 — **the extension on its own, with no CLI anywhere, is a state a
 * user can really be in, and this drives it in a real browser.**
 *
 * Topology principle 8 says a user may install the extension and never install
 * the helper — or install it months later. Everything that state needs is inside
 * the extension, because the menu-bar app and the dashboard both need the CLI and
 * therefore do not exist yet.
 *
 * ## Why this has to be a browser spec
 *
 * The claims here are about **wiring and rendering**: that the popup, on a real
 * profile with no native host registered, actually shows the onboarding card and
 * the persistent notice, that the copy button copies the exact install line, and
 * that nothing on screen tells the user to run a `chat-stasher …` command. The
 * unit suite (tests/w214-extension-only.test.ts) pins the decision functions and
 * never loads a page; this file pins what a user would see.
 *
 * 🔴 The state is real: no native messaging host is registered on this machine or
 *    on the profile this spec launches, so `hello` genuinely fails. Nothing here
 *    stubs the host's absence — the fixtures only put captures in the spool, which
 *    a spec cannot do by chatting.
 *
 * ## The three states, and the wall between them
 *
 *   · **never connected** — no `hello` has ever succeeded: onboarding, the
 *     installer, and never a `chat-stasher …` command.
 *   · **was connected, now broken** — a previous `hello` is on record: an alarm,
 *     no onboarding card, and the `chat-stasher install-native-host …` command is
 *     allowed because something once proved the CLI exists.
 *   · **delivered N** — the record background writes when the backlog drains on a
 *     host that turned up.
 */

import { expect } from '@playwright/test';
// The cap comes from the product, not a second copy here: a spec that spelled
// 256 MiB out again would keep passing after the real cap changed.
import { OUTBOX_CAPACITY_BYTES } from '../lib/outbox';
import { readStorage, seedOutbox, writeStorage, test, type Extension, type OutboxEntry } from './harness';

/** The one line the extension may offer a user who has no CLI. */
const INSTALLER = 'curl -fsSL https://chatstasher.com/install.sh | sh';

/** A stage path a *previous* `hello` reported — the evidence the CLI exists. */
const LAST_KNOWN_STAGE = '/Users/me/stage';
const HOST_STATUS_KEY = 'cs_native_host_status_v1';
const CONNECT_DELIVERY_KEY = 'cs_outbox_connect_delivery_v1';

/**
 * One capture in the spool, with `bytes` set to whatever the case needs.
 *
 * 🔴 `bytes` is the product's own byte accounting and the only thing its summary
 *    reads, so a case that wants "a spool that is 82% full" seeds that number
 *    instead of 210 MiB of payload. Nothing here is ever delivered: no host is
 *    installed in this profile, which is the whole point of the file.
 */
function capture(bytes: number, n = 1): OutboxEntry {
  const sha = String(n).padStart(64, 'a');
  return {
    sha256: sha,
    name: `chatgpt-${sha.slice(0, 8)}-1111-2222-3333-444444444444.json`,
    payload: `{"sessionId":"${sha.slice(0, 8)}-1111-2222-3333-444444444444"}`,
    bytes,
    enqueuedAt: Date.now() - 1000,
    attempts: 0,
    lastError: null,
    lastAttemptAt: null,
    state: 'pending',
  };
}

/** The popup, as a real page. Its own entrypoint runs and does the rest. */
async function openPopup(ext: Extension, initScript?: () => void) {
  const page = await ext.context.newPage();
  if (initScript) await page.addInitScript(initScript);
  await page.goto(`chrome-extension://${ext.extensionId}/popup.html`, { waitUntil: 'domcontentloaded' });
  // The first paint happens after the background probe answers, so every case
  // waits for the element it is about rather than for a fixed delay.
  return page;
}

/** Is this element actually on screen? `hidden` is how this popup hides things. */
async function visible(page: Awaited<ReturnType<typeof openPopup>>, id: string): Promise<boolean> {
  return await page.evaluate((elementId: string) => {
    const el = document.getElementById(elementId);
    return el !== null && !(el as HTMLElement).hidden;
  }, id);
}

test('never connected: the onboarding card, the installer, and no `chat-stasher …` command', async ({ ext }) => {
  await seedOutbox(ext, [capture(1024)]);
  const popup = await openPopup(ext);

  // The card is the first thing this user needs, and it carries the one line.
  await expect.poll(() => visible(popup, 'first-run')).toBe(true);
  await expect.poll(async () => (await popup.textContent('#first-run-command')) ?? '').toBe(INSTALLER);
  await expect.poll(async () => (await popup.textContent('#copy-installer')) ?? '').toBe('Copy');

  // 🔴 The rule the whole task turns on: with nothing on record that a helper ever
  //    answered here, no `chat-stasher …` command may appear anywhere on the page.
  //    The one-line installer is the first step instead.
  const text = (await popup.textContent('body')) ?? '';
  expect(text).toContain(INSTALLER);
  expect(text).not.toContain('chat-stasher install-native-host');
  expect(text).not.toContain('install-native-host');

  await popup.close();
});

test('never connected: the persistent "only in this browser" notice and the usage bar', async ({ ext }) => {
  await seedOutbox(ext, [capture(1024)]);
  const popup = await openPopup(ext);

  await expect.poll(() => visible(popup, 'only-in-browser')).toBe(true);
  expect((await popup.textContent('#only-in-browser-title')) ?? '')
    .toContain('Only in this browser — not a backup yet');
  // Never-connected wording: the helper has never answered here.
  expect((await popup.textContent('#only-in-browser-reason')) ?? '')
    .toContain('No helper has ever answered on this machine');

  // The usage bar is shown while there is anything staged, with the byte count the
  // product measured — not a zero and not a percentage invented by the view.
  await expect.poll(() => visible(popup, 'outbox-bar')).toBe(true);
  const caption = (await popup.textContent('#outbox-bar-caption')) ?? '';
  expect(caption).toContain('KiB');
  expect(caption).toContain('MiB');
  // A spool this empty is nowhere near the line, so it draws no state sentence.
  expect(((await popup.textContent('#outbox-bar-state')) ?? '').trim()).toBe('');

  await popup.close();
});

test('an 82%-full spool: the near-full sentence, and it is not the full one', async ({ ext }) => {
  await seedOutbox(ext, [capture(Math.ceil(OUTBOX_CAPACITY_BYTES * 0.82))]);
  const popup = await openPopup(ext);

  await expect.poll(() => visible(popup, 'outbox-bar')).toBe(true);
  await expect.poll(async () => ((await popup.textContent('#outbox-bar-state')) ?? '').length).toBeGreaterThan(0);
  const state = (await popup.textContent('#outbox-bar-state')) ?? '';
  expect(state).toContain('80%');
  expect(state).toContain('backfill');
  // Near-full is a pause, not a refusal: it must not borrow the full sentence.
  expect(state).not.toContain('FULL');
  expect(state).not.toContain('refused');

  // The fill carries the near-full style and not the full one, so amber and red
  // cannot be confused on screen.
  const classes = await popup.evaluate(() => {
    const fill = document.getElementById('outbox-bar-fill');
    return fill ? [...fill.classList] : [];
  });
  expect(classes).toContain('near');
  expect(classes).not.toContain('full');

  await popup.close();
});

test('a full spool: the red refusal sentence, and nothing queued was deleted', async ({ ext }) => {
  await seedOutbox(ext, [capture(OUTBOX_CAPACITY_BYTES)]);
  const popup = await openPopup(ext);

  await expect.poll(() => visible(popup, 'outbox-bar')).toBe(true);
  const state = (await popup.textContent('#outbox-bar-state')) ?? '';
  expect(state).toContain('FULL');
  expect(state).toContain('refused');
  expect(state).toContain('Nothing queued was deleted');

  const classes = await popup.evaluate(() => {
    const fill = document.getElementById('outbox-bar-fill');
    const line = document.getElementById('outbox-bar-state');
    return { fill: fill ? [...fill.classList] : [], line: line ? [...line.classList] : [] };
  });
  expect(classes.fill).toContain('full');
  expect(classes.line).toContain('red');

  await popup.close();
});

test('was connected, now broken: an alarm, not onboarding — and the CLI command is allowed', async ({ ext }) => {
  // A `hello` that once succeeded, recorded the way the product records it. The
  // path is what proves the CLI exists on this machine.
  await writeStorage(ext, {
    [HOST_STATUS_KEY]: {
      at: Date.now() - 86_400_000,
      ok: true,
      stage: LAST_KNOWN_STAGE,
      machine: 'fixture-machine',
      hostVersion: '0.3.0',
      lastKnownStage: LAST_KNOWN_STAGE,
    },
  });
  await seedOutbox(ext, [capture(1024)]);
  const popup = await openPopup(ext);

  // The notice is still there — these captures are still not a backup — but it is
  // the other sentence, and the onboarding card is gone.
  await expect.poll(() => visible(popup, 'only-in-browser')).toBe(true);
  expect((await popup.textContent('#only-in-browser-reason')) ?? '')
    .toContain('has answered before and does not answer now');
  expect(await visible(popup, 'first-run')).toBe(false);

  // 🔴 The evidence rule, from the other side: because a helper has been seen on
  //    this machine, the fix command may name the CLI — with the stage it reported.
  const text = (await popup.textContent('body')) ?? '';
  expect(text).toContain(`chat-stasher install-native-host --stage ${LAST_KNOWN_STAGE}`);
  // And the installer is not offered as though the CLI were missing.
  expect(text).not.toContain(INSTALLER);

  await popup.close();
});

test('delivered N: the record background writes when the backlog drains is shown', async ({ ext }) => {
  await writeStorage(ext, { [CONNECT_DELIVERY_KEY]: { at: Date.now() - 60_000, count: 5 } });
  const popup = await openPopup(ext);

  await expect.poll(() => visible(popup, 'delivered')).toBe(true);
  const line = (await popup.textContent('#delivered')) ?? '';
  expect(line).toContain('5');
  expect(line).toContain('archive');

  await popup.close();
});

test('🔴 "Export now" really exports — it is the one escape hatch that must work with no helper', async ({ ext }) => {
  // The card exists to rescue a user who cannot deliver. If its export button were
  // decorative, this state would have no way out at all, so the click is driven
  // rather than the label asserted.
  await seedOutbox(ext, [capture(1024, 1), capture(2048, 2)]);
  const popup = await openPopup(ext);
  await expect.poll(() => visible(popup, 'first-run')).toBe(true);

  await popup.click('#first-run-export');

  // The export books itself in storage (lib/outbox.ts's `recordExport`), and the
  // record is what the popup's own "last export" line renders from — so a written
  // record plus a repainted line is the export having happened, with both entries.
  await expect.poll(async () => {
    const stored = await readStorage(ext, null);
    return (stored['cs_outbox_last_export_v1'] as { entries?: number } | undefined)?.entries ?? 0;
  }).toBe(2);
  await expect.poll(async () => (await popup.textContent('#last-export')) ?? '').toContain('2');

  // Nothing is delivered by exporting: the captures stay queued until a host acks
  // them (spec §10), and this profile has no host.
  const stored = await readStorage(ext, null);
  const rec = stored['cs_outbox_last_export_v1'] as { filename?: string; bytes?: number };
  expect(rec.filename).toBeTruthy();
  // The file is JSONL: one line per entry, so the byte count is the payload bytes
  // **plus one newline each** — `bytes += entry.bytes + 1` (lib/outbox.ts:658).
  // 1024 + 2048 payload, + 2 newlines = 3074. Asserted exactly, because a count
  // that merely exceeded the payload would also pass if an entry were written twice.
  expect(rec.bytes).toBe(1024 + 2048 + 2);

  await popup.close();
});

test('the copy button copies the exact installer line, and claims it only when the write worked', async ({ ext }) => {
  await seedOutbox(ext, [capture(1024)]);
  /**
   * 🔴 The clipboard cannot be read back here: Chrome refuses to grant clipboard
   *    permissions to a `chrome-extension://` origin ("Permission can't be granted
   *    to opaque origins"), so `navigator.clipboard.readText()` is not available to
   *    the test in this context.
   *
   *    So the observation happens from the other side: the two write paths the
   *    product actually uses are wrapped — `navigator.clipboard.writeText` first,
   *    and the `document.execCommand('copy')` textarea fallback second — and each
   *    records the text it was handed **and still performs the real write**. A spy
   *    that replaced the implementation would leave this test asserting its own
   *    stub; these delegate, so the button's own "Copied" claim still depends on
   *    the browser accepting the write.
   */
  const spy = () => {
    const w = window as unknown as { __csCopied?: string[] };
    w.__csCopied = [];
    const clipboard = navigator.clipboard;
    if (clipboard?.writeText) {
      const real = clipboard.writeText.bind(clipboard);
      clipboard.writeText = async (text: string) => {
        w.__csCopied!.push(text);
        return await real(text);
      };
    }
    const realExec = document.execCommand.bind(document);
    document.execCommand = ((command: string) => {
      if (command === 'copy') {
        const active = document.activeElement as HTMLTextAreaElement | null;
        if (active && typeof active.value === 'string') w.__csCopied!.push(active.value);
      }
      return realExec(command);
    }) as typeof document.execCommand;
  };

  const popup = await openPopup(ext, spy);
  await expect.poll(() => visible(popup, 'first-run')).toBe(true);

  await popup.click('#copy-installer');

  // The button may only say "Copied" once a write really succeeded — that label is
  // the assertion that the browser accepted it, not decoration.
  await expect.poll(async () => (await popup.textContent('#copy-installer')) ?? '').toBe('Copied');
  // And what it handed over is exactly the one-line installer, byte for byte.
  const copied = await popup.evaluate(() => (window as unknown as { __csCopied?: string[] }).__csCopied ?? []);
  expect(copied).toEqual([INSTALLER]);

  await popup.close();
});
