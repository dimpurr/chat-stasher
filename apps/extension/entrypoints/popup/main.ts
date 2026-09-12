/**
 * C18 · The popup's wiring.
 *
 * Since W2 it grew three things, and each one displays **a fact that already
 * exists**, never a guess:
 *  1. the delivery channel: the answer background got the last time it asked
 *     `hello` (including stage/machine/version), or a named failure reason plus
 *     the fix command. The popup has not one line of probing code of its own.
 *  2. the outbox: read straight from the IndexedDB under this same extension
 *     origin — the same data the drain in background reads.
 *  3. "Export undelivered captures": Blob + `<a download>`, **with no downloads
 *     permission**. The user's click is the user gesture, so the browser allows
 *     the download.
 *
 * Plus the language selector: the switch that makes the popup render in English
 * or Chinese regardless of the browser's own language (lib/i18n.ts explains why
 * a layer is needed for that at all). The choice is persisted in
 * `storage.local`, and a change repaints immediately — including the selector's
 * own labels, because those are shown in each language's own script.
 *
 * 🔴 This file **never** issues a network request and **never** triggers a
 *    backfill. Flipping the switch writes one boolean; the real tick is still
 *    only woken by the two heartbeats.
 */

import {
  browserLocalStore,
  browserLocalSnapshot,
} from '../../lib/backfill/store';
import {
  isBackfillEnabled,
  setBackfillEnabled,
  tickBlockReason,
} from '../../lib/backfill/schedule';
import {
  loadLastTick,
  loadTargets,
  syncBackfillAlarm,
  type AlarmsApi,
} from '../../lib/backfill/alarm';
import {
  backfillStateEntries,
  collectFailures,
  pickBackfillState,
  renderPopup,
  summarizeOutbox,
  POPUP_START_BACKFILL_MESSAGE,
  POPUP_STATUS_MESSAGE,
  type BackfillRuntimeStatus,
  type PopupModel,
  type PopupView,
} from '../../lib/popup-view';
import { clearFailures } from '../../lib/backfill/failures';
import {
  buildExportFile,
  listEntries,
  loadLastExport,
  recordExport,
  undeliveredEntries,
} from '../../lib/outbox';
import { loadHostPause, loadHostStatus } from '../../lib/host-status';
import { exportNoHistory, exportNothingQueued, exportUnreadable } from '../../lib/ui-strings';
import { initUiLocale, normalizeUiLocale, setUiLocale, type UiLocale } from '../../lib/i18n';

/**
 * Ask background for the runtime facts. When the answer does not come (the SW
 * will not start, nobody is listening) we answer **in the most conservative
 * direction: not connected** — better to show "not running" than to show it as
 * running.
 */
async function askBackground(): Promise<BackfillRuntimeStatus> {
  try {
    const reply = await browser.runtime.sendMessage({ type: POPUP_STATUS_MESSAGE });
    if (reply && typeof (reply as BackfillRuntimeStatus).transportWired === 'boolean') {
      return reply as BackfillRuntimeStatus;
    }
  } catch (err) {
    console.warn('[chat-stasher] popup status query failed', (err as Error).message);
  }
  return { transportWired: false, lastTickReason: null, liveTarget: null };
}

async function collect(): Promise<PopupModel> {
  const store = browserLocalStore();
  const runtime = await askBackground();

  const enabled = await isBackfillEnabled(store);

  let snapshot: Record<string, unknown> | null = null;
  try {
    snapshot = await browserLocalSnapshot();
  } catch (err) {
    console.warn('[chat-stasher] popup snapshot read failed', (err as Error).message);
  }
  const state = pickBackfillState(snapshot);

  // 🔴 C30 · The backfill-target registry. This is the **only** source of
  //    targets for the alarm's path.
  const targets = await loadTargets(store);
  const lastTick = await loadLastTick(store);

  // 🔴 W2 · The outbox. listEntries() returning null = unreadable ⇒ say it is
  //    unreadable, never show it as "empty" (that would record an unknown as empty).
  let outbox: PopupModel['outbox'];
  try {
    const entries = await listEntries();
    outbox = entries === null ? null : summarizeOutbox(entries);
  } catch (err) {
    console.warn('[chat-stasher] popup outbox read failed', (err as Error).message);
    outbox = null;
  }
  const lastExport = await loadLastExport(store);
  const hostPause = await loadHostPause(store);
  // Prefer the channel status background just fetched; fall back to the last
  // conclusion in storage when it did not come back.
  const nativeHost = runtime.nativeHost ?? await loadHostStatus(store);

  // 🔴 The one predicate shared with tickBackfill, so the ordering is identical by construction.
  const block = await tickBlockReason({
    hasStore: store !== null,
    isEnabled: () => enabled,
    isHostPaused: async () => hostPause !== null,
    hasHttp: runtime.transportWired,
    hasTargets: targets.length > 0,
  });

  return {
    enabled,
    block,
    state,
    target: state ? { platform: state.platform, scope: state.scope } : null,
    // 🔴 C20: aggregated across every platform/account. An unreadable snapshot ⇒
    //    empty list (at that point we genuinely know nothing).
    failures: collectFailures(snapshot),
    lastTick,
    // 🔴 C33 · The two preconditions of the "start backfilling this platform"
    //    button, both of them **facts**, not inferences.
    liveTarget: runtime.liveTarget ?? null,
    targetCount: targets.length,
    nativeHost,
    outbox,
    hostPause,
    lastExport,
  };
}

/**
 * 🔴 C20 · "Got it — clear this list".
 * Walks every debt set in the snapshot, zeroes failures / failuresDropped and
 * writes them back. **It triggers no re-fetch whatsoever** — that is the product
 * decision of "no retry"; the button only means "I have seen it".
 */
async function onClearFailures(): Promise<void> {
  const store = browserLocalStore();
  if (!store) return;
  let snapshot: Record<string, unknown> | null = null;
  try {
    snapshot = await browserLocalSnapshot();
  } catch (err) {
    console.warn('[chat-stasher] popup snapshot read failed', (err as Error).message);
    return;
  }
  for (const { key, state } of backfillStateEntries(snapshot)) {
    if (state.failures === undefined && !state.failuresDropped) continue;
    clearFailures(state);
    await store.save(key, state);
  }
  await refresh();
}

/**
 * 🔴 C33 · The user pressed "start backfilling this platform".
 *
 * Registration is left to background rather than writing storage here directly:
 * only background can ping, live, for "which channel is alive right now". The
 * liveTarget the popup holds is a snapshot taken when it opened, and registering
 * from it would be treating a possibly stale fact as current — which is exactly
 * "guessing".
 */
async function onStartBackfill(): Promise<void> {
  let reply: unknown = null;
  try {
    reply = await browser.runtime.sendMessage({ type: POPUP_START_BACKFILL_MESSAGE });
  } catch (err) {
    console.warn('[chat-stasher] popup start-backfill failed', (err as Error).message);
  }
  const ok = !!reply && (reply as { ok?: boolean }).ok === true;
  if (!ok) {
    console.warn('[chat-stasher] backfill target not registered:',
      (reply as { reason?: string } | null)?.reason ?? 'no reply');
  }
  await refresh();
}

/**
 * 🔴 W2 · "Export undelivered captures".
 *
 * Spec §8: the file name is `chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl`,
 * one payload per line (verbatim) followed by a `\n`.
 *
 * 🔴 The user's click is the user gesture, so `<a download>` is permitted — and
 *    **no** `downloads` permission is used here (it has been removed from the
 *    manifest; see wxt.config.ts).
 *
 * 🔴 Exporting **does not delete** any entry: they are still queued in the
 *    outbox, and once the host is reachable again they are confirmed as
 *    duplicates and cleared (§7 — content addressing makes re-sending safe).
 */
async function onExportUndelivered(): Promise<void> {
  let entries;
  try {
    entries = await undeliveredEntries();
  } catch (err) {
    console.warn('[chat-stasher] popup outbox read failed', (err as Error).message);
    entries = null;
  }
  if (entries === null) {
    // Unreadable ⇒ do **not** produce a file: conjuring an empty one would tell
    // the user "there is nothing undelivered".
    setExportNote(exportUnreadable());
    return;
  }
  if (entries.length === 0) {
    // Empty outbox: produce no file at all, just say so.
    setExportNote(exportNothingQueued());
    return;
  }

  const at = Date.now();
  const file = buildExportFile(entries, at);
  const blob = new Blob([file.content], { type: 'application/x-ndjson' });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = file.filename;
  anchor.style.display = 'none';
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // Revoke late: revoking too early makes the download come up empty (Firefox especially).
  setTimeout(() => URL.revokeObjectURL(url), 30_000);

  await recordExport(browserLocalStore(), {
    at,
    entries: file.entries,
    bytes: file.bytes,
    filename: file.filename,
  });
  await refresh();
}

function setExportNote(text: string): void {
  const el = document.getElementById('last-export');
  if (el) el.textContent = text;
}

function text(id: string, value: string): void {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

/**
 * Paint the language selector. The options are rebuilt on every paint because
 * their labels are written in their own languages and the label of "Language"
 * follows the active one — so switching has to be able to relabel itself.
 */
function paintLocale(view: PopupView): void {
  text('locale-label', view.locale.label);
  const select = document.getElementById('locale') as HTMLSelectElement | null;
  if (!select) return;
  const wanted = view.locale.options.map((o) => o.value).join(',');
  const current = Array.from(select.options).map((o) => o.value).join(',');
  if (wanted !== current) {
    select.textContent = '';
    for (const option of view.locale.options) {
      const el = document.createElement('option');
      el.value = option.value;
      el.textContent = option.label;
      select.appendChild(el);
    }
  } else {
    for (const option of view.locale.options) {
      const el = Array.from(select.options).find((o) => o.value === option.value);
      if (el && el.textContent !== option.label) el.textContent = option.label;
    }
  }
  select.value = view.locale.value;
  // The document's language attribute follows the UI so that hyphenation and
  // screen readers are not told the wrong script.
  document.documentElement.lang = view.locale.value === 'zh_CN' ? 'zh-CN' : 'en';
}

function paint(view: PopupView): void {
  paintLocale(view);
  text('status', view.status);
  text('channel', view.channel);
  // 🔴 W2: the pause row, the outbox row and the export row only appear when
  //    they have content — never an empty shell that reads as "this was
  //    supposed to be empty".
  text('pause', view.pause ?? '');
  const pauseBox = document.getElementById('pause');
  if (pauseBox) pauseBox.hidden = view.pause === null;

  text('outbox', view.outbox ?? '');
  const outboxBox = document.getElementById('outbox');
  if (outboxBox) outboxBox.hidden = view.outbox === null;

  text('last-export', view.lastExport || exportNoHistory());
  const exportBtn = document.getElementById('export-file') as HTMLButtonElement | null;
  if (exportBtn) {
    exportBtn.textContent = view.exportFile.label;
    exportBtn.hidden = !view.exportFile.visible;
  }

  // 🔴 C20: when there are failures this row must be in the most prominent
  //    place; when there are none the whole block is hidden.
  text('failures', view.failures ?? '');
  const failBox = document.getElementById('failures');
  if (failBox) failBox.hidden = view.failures === null;

  const clearBtn = document.getElementById('clear-failures') as HTMLButtonElement | null;
  if (clearBtn) {
    clearBtn.textContent = view.clearFailures.label;
    clearBtn.hidden = !view.clearFailures.visible;
  }

  const startBtn = document.getElementById('start-backfill') as HTMLButtonElement | null;
  if (startBtn) {
    startBtn.textContent = view.startBackfill.label;
    startBtn.hidden = !view.startBackfill.visible;
  }

  text('running', view.running);
  text('missing', view.missing ?? '');
  text('progress', view.progress);
  // 🔴 C22: which platforms can have their history backfilled and which cannot. Always shown.
  text('coverage', view.coverage);
  text('toggle-label', view.toggle.label);

  const toggle = document.getElementById('toggle') as HTMLInputElement | null;
  if (toggle) {
    toggle.checked = view.toggle.checked;
    toggle.disabled = view.toggle.disabled;
  }

  const notes = document.getElementById('notes');
  if (notes) {
    notes.textContent = '';
    for (const note of view.notes) {
      const p = document.createElement('p');
      // textContent (not innerHTML): the wording can carry a user's account
      // scope, which must never be parsed as HTML.
      p.textContent = note;
      notes.appendChild(p);
    }
  }
}

async function refresh(): Promise<void> {
  paint(renderPopup(await collect()));
}

async function onToggle(on: boolean): Promise<void> {
  const store = browserLocalStore();
  // If it cannot be saved, do not pretend the flip worked: repaint at once and
  // the UI falls back to the real value.
  await setBackfillEnabled(store, on);
  // 🔴 C19: the switch and the alarm must change together. The **stored value**
  //    is authoritative, not `on` — if the storage write failed the switch falls
  //    back, and the alarm has to fall back with it, so that "switch is off but
  //    the alarm is still firing" cannot happen.
  const persisted = await isBackfillEnabled(store);
  const result = await syncBackfillAlarm(
    (browser as unknown as { alarms?: AlarmsApi }).alarms ?? null,
    persisted,
  );
  console.log('[chat-stasher] backfill alarm ->', result);
  await refresh();
}

/** 🔴 The language switch: persist the choice, then repaint in the new language immediately. */
async function onLocaleChange(value: string): Promise<void> {
  const applied: UiLocale = await setUiLocale(normalizeUiLocale(value));
  console.log('[chat-stasher] ui locale ->', applied);
  await refresh();
}

document.getElementById('toggle')?.addEventListener('change', (ev) => {
  const on = (ev.target as HTMLInputElement).checked;
  void onToggle(on).catch((err) => {
    console.warn('[chat-stasher] popup toggle failed', (err as Error).message);
    void refresh();
  });
});

document.getElementById('locale')?.addEventListener('change', (ev) => {
  const value = (ev.target as HTMLSelectElement).value;
  void onLocaleChange(value).catch((err) => {
    console.warn('[chat-stasher] popup language switch failed', (err as Error).message);
    void refresh();
  });
});

document.getElementById('start-backfill')?.addEventListener('click', () => {
  void onStartBackfill().catch((err) => {
    console.warn('[chat-stasher] popup start-backfill failed', (err as Error).message);
    void refresh();
  });
});

document.getElementById('export-file')?.addEventListener('click', () => {
  void onExportUndelivered().catch((err) => {
    console.warn('[chat-stasher] popup export failed', (err as Error).message);
    void refresh();
  });
});

document.getElementById('clear-failures')?.addEventListener('click', () => {
  void onClearFailures().catch((err) => {
    console.warn('[chat-stasher] popup clear-failures failed', (err as Error).message);
    void refresh();
  });
});

// Load the stored language before the first paint, so the popup does not flash
// the browser's language and then swap to the chosen one.
void initUiLocale()
  .catch((err) => {
    console.warn('[chat-stasher] popup locale init failed', (err as Error).message);
  })
  .then(() => refresh())
  .catch((err) => {
    console.error('[chat-stasher] popup render failed', (err as Error).message);
  });
