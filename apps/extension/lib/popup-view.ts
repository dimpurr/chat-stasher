/**
 * C18 · The popup's **pure render layer**.
 *
 * Why it is a file of its own, and why it does not touch the DOM once:
 *  entrypoints/popup/main.ts cannot be run under node, and this task's single
 *  most important criterion — "open but with no port must NEVER be shown as
 *  archiving" — has to be assertable. So *what is displayed* all lives here, and
 *  main.ts only fetches the data and puts it into the DOM.
 *
 * 🔴 Three hard rules:
 *  1. Progress wording **always reuses `formatProgress` from
 *     lib/backfill/progress.ts**. This file must not compute a percentage or
 *     assemble a '%' itself — a second copy would be a way around C11's "no
 *     percentage when the denominator is untrustworthy" rule.
 *  2. "On" and "running" are two different things and must be said on two
 *     separate lines. The switch being on does not mean it is doing anything.
 *  3. No "about N days / N hours left" anywhere. We have no rate model;
 *     inventing one would be a lie.
 *
 * Every user-visible sentence is resolved through `t` (lib/i18n.ts) at render
 * time, so the same model renders in whichever language the popup is set to.
 */

import { currentUiLocale, t, type UiLocale } from './i18n';
import { formatProgress, retryMinutesLeft } from './backfill/progress';
import {
  describeFailureReason,
  droppedOf,
  failuresOf,
  MAX_FAILURES,
  type FailureEntry,
} from './backfill/failures';
import {
  BACKFILL_ALARM_PERIOD_MINUTES,
  type BackfillTickRecord,
} from './backfill/alarm';
import {
  BACKFILL_PARTIAL,
  BACKFILL_SUPPORTED_PLATFORMS,
  BACKFILL_UNSUPPORTED,
  BACKFILL_UNSUPPORTED_PLATFORMS,
} from './backfill/enumerate';
import { DEFAULT_DETAIL_PACE } from './backfill/pace';
import type { TickBlockReason } from './backfill/schedule';
import { haltClassOf, stateKey, BACKFILL_STATE_VERSION, type BackfillState } from './backfill/types';
import type { HostPauseRecord, HostStatusRecord } from './host-status';
import type { LastExport, OutboxEntry } from './outbox';
import { OUTBOX_CAPACITY_BYTES } from './outbox';
import * as ui from './ui-strings';

/**
 * Popup ↔ background message types.
 * The constants live here rather than in background.ts: the popup only needs to
 * import one string, and it should not drag the whole background module (with
 * download / badge / engine behind it) into the popup's bundle.
 */
export const POPUP_STATUS_MESSAGE = 'cs-backfill-status';

/**
 * 🔴 C33 · Popup → background: "backfill THIS platform, I am saying so".
 *
 * Why a second registration entry point is needed: backfill targets used to be
 * registered on exactly one occasion, the kickBackfill call that follows a live
 * capture. That limitation was deliberate and this change does not touch it
 * (lib/backfill/alarm.ts:80-87 — when the alarm wakes, the SW is brand new, and
 * it does not guess: it only uses what is already there).
 * But the corollary is: a user who has just installed the extension, has a
 * platform page open and has not yet had a conversation, will **never** start
 * backfilling. ⇒ The fix is not to start guessing, it is to let the user say it
 * **once, explicitly**. The "we do not guess" principle is therefore intact:
 * a target either comes from a real capture or from the user pressing this.
 * Neither is invented by us.
 */
export const POPUP_START_BACKFILL_MESSAGE = 'cs-backfill-start-here';

/** The runtime facts background hands back to the popup. */
export interface BackfillRuntimeStatus {
  /**
   * 🔴 Whether the fetch channel is actually connected.
   * Under C18 this was always false (no production code injected a port). Since
   * C19 it is the result of background **pinging right now**: is there a live,
   * logged-in platform tab that can fetch on our behalf at this moment. Still a
   * fact, not an inference.
   */
  transportWired: boolean;
  /** The conclusion of the most recent tick; null again once the SW is reclaimed — which is itself the truth. */
  lastTickReason: string | null;
  /**
   * 🔴 C33 · **Which platform / which origin the live channel belongs to right now.**
   * It comes from the same live ping as transportWired (the same pickLiveTab), so
   * "there is a channel but I cannot say which platform" cannot happen.
   * null = no live channel, or the origin is not in the platform table — either
   * way the button must not be shown.
   */
  liveTarget?: { platform: string; origin: string } | null;
  /**
   * 🔴 W2 · The conclusion of the most recent `hello` (§6.1), written to storage
   * by background after it asks. It is **a record of one question and answer**,
   * with a timestamp — what the popup shows is "the answer we got last time",
   * not "our guess right now". null / omitted = never asked.
   */
  nativeHost?: HostStatusRecord | null;
}

export interface PopupModel {
  /** The persisted value of the switch. */
  enabled: boolean;
  /**
   * 🔴 The first of the four gates to block, from lib/backfill/schedule.ts's
   * tickBlockReason — the **same function** the runtime tickBackfill uses.
   * null means all four passed.
   */
  block: TickBlockReason | null;
  /** The debt set. null means storage has no such set yet (it has never run). */
  state: BackfillState | null;
  /** Which platform / which account this progress belongs to. null when it cannot be told. */
  target: { platform: string; scope: string } | null;
  /**
   * 🔴 C20 · The on-disk failure list (**aggregated across every platform/account**).
   * Why aggregate instead of just looking at model.state: the progress row picks
   * a single set to display (the one with the most archived). If failures only
   * looked at that same set, what was lost under another account would vanish
   * from the UI — which is precisely "showing everything as fine".
   */
  failures: FailureSummary;
  /**
   * 🔴 C30 · The trace of the alarm's most recent tick (read from storage, not
   * from memory). null = storage has no such record yet — which is itself the
   * truth (the alarm has not woken once, or the extension was just installed and
   * it is not due), and it is said as-is, never invented.
   */
  lastTick?: BackfillTickRecord | null;
  /**
   * 🔴 C33 · The platform/origin the live channel belongs to right now (from
   * BackfillRuntimeStatus). Omitted/null ⇒ no usable channel ⇒ the "start
   * backfilling this platform" button does not appear (pressing it would do
   * nothing).
   */
  liveTarget?: { platform: string; origin: string } | null;
  /**
   * 🔴 C33 · How many backfill targets are already in the registry.
   * Omitted ⇒ treat as 0? **No**: omitted is treated as "unknown", which is
   * treated as "there are some", so the button does not appear — existing call
   * sites (including existing tests) need not change a single character, and no
   * extra button appears out of nowhere.
   */
  targetCount?: number;
  /**
   * 🔴 W2 · The facts from the most recent `hello` (§6.1).
   * null / omitted ⇒ never asked ⇒ the wording says "never asked" as-is, never a guess.
   */
  nativeHost?: HostStatusRecord | null;
  /**
   * 🔴 W2 · The outbox as it stands. null = could not be read (IndexedDB
   * unavailable) ⇒ say so as-is. Omitted ⇒ treat as empty: existing call sites
   * (including existing tests) need not change a single character, and an empty
   * outbox genuinely means "not one item is waiting".
   */
  outbox?: PopupOutbox | null;
  /** 🔴 W2 · The record of backfill pausing because the host is unreachable; null = not paused. */
  hostPause?: HostPauseRecord | null;
  /** 🔴 W2 · The most recent export (read from storage.local); null = never exported. */
  lastExport?: LastExport | null;
  /**
   * 🔴 W13 · The moment the wording is computed for (`Date.now()` ms), so that
   * "about M minutes" in a transient-retry line is a number that can be asserted.
   * Omitted ⇒ the real clock — the same optional-field pattern as the fields above,
   * so no existing call site changes a character. It is never used to decide
   * *whether* to retry; that decision belongs to the engine's clock alone.
   */
  now?: number;
}

/** How the outbox is presented: only the numbers the UI needs, never the payload. */
export interface PopupOutbox {
  pending: number;
  rejected: number;
  bytes: number;
  capacityBytes: number;
  full: boolean;
  rejectedKinds: Array<{ kind: string; count: number }>;
  rejectedSamples: Array<{ kind: string; detail: string }>;
}

/** How many rejected entries to list in plain language — beyond that, go read the export file, not the popup. */
export const MAX_REJECTED_SAMPLES = 5;

/**
 * 🔴 Pure: outbox entries → the few numbers the popup displays.
 * The render layer never walks the entries, and the payload never comes along:
 * the wording needs counts and kinds only.
 */
export function summarizeOutbox(
  entries: readonly OutboxEntry[],
  capacityBytes: number = OUTBOX_CAPACITY_BYTES,
): PopupOutbox {
  const rejected = entries.filter((e) => e.state === 'rejected');
  const counts = new Map<string, number>();
  for (const entry of rejected) {
    const kind = entry.rejectKind ?? entry.lastError ?? ui.OUTBOX_KIND_UNKNOWN;
    counts.set(kind, (counts.get(kind) ?? 0) + 1);
  }
  const bytes = entries.reduce((sum, e) => sum + (Number.isFinite(e.bytes) ? e.bytes : 0), 0);
  return {
    pending: entries.filter((e) => e.state === 'pending').length,
    rejected: rejected.length,
    bytes,
    capacityBytes,
    full: bytes >= capacityBytes,
    rejectedKinds: [...counts.entries()]
      .map(([kind, count]) => ({ kind, count }))
      .sort((a, b) => (b.count - a.count) || a.kind.localeCompare(b.kind)),
    rejectedSamples: rejected.slice(0, MAX_REJECTED_SAMPLES).map((entry) => ({
      kind: entry.rejectKind ?? ui.OUTBOX_KIND_UNKNOWN,
      // Summary: keep the reason code itself. 🔴 Never the payload / URL / conversation body.
      detail: (entry.lastError ?? ui.OUTBOX_DETAIL_MISSING).slice(0, 200),
    })),
  };
}

/** The aggregated failure list. `entries` is already sorted newest first. */
export interface FailureSummary {
  entries: FailureEntry[];
  /** How many earlier failures were dropped for exceeding the cap. 🔴 Never a silent truncation. */
  dropped: number;
}

/** One language choice in the popup's selector. */
export interface LocaleOption {
  value: UiLocale;
  /** Written in its own language, so a user who cannot read the current one can still find theirs. */
  label: string;
}

export interface PopupView {
  /** Line 1: what state the switch itself is in. */
  status: string;
  /** W2: the delivery-channel line. Connected ⇒ stage / machine / host version; not ⇒ a named reason + the fix command. */
  channel: string;
  /** W2: the outbox line (waiting / rejected / capacity). null when the outbox is empty and not full. */
  outbox: string | null;
  /** W2: the backfill-paused line (host unreachable). null when not paused. */
  pause: string | null;
  /** W2: the export button's wording and visibility. */
  exportFile: { label: string; visible: boolean };
  /** W2: the note about the most recent export; "never" when there has not been one. */
  lastExport: string;
  /**
   * 🔴 C20 · Line 2: **when something was not stored, this line must appear.**
   * It is null when there are no failures (that is when "everything is fine" is
   * the truth). Its position before "is it running" is deliberate: a leg running
   * smoothly must not paper over "something was lost".
   */
  failures: string | null;
  /** Line 3: 🔴 whether it is actually running. */
  running: string;
  /** Line 3: which gate it is stuck on and what is missing. null when nothing is. */
  missing: string | null;
  /** Line 4: progress (reuses C11). */
  progress: string;
  /**
   * 🔴 C22 · Line 5: **which platforms can have their history backfilled and
   * which cannot, for now.**
   *
   * Why this line has to exist: a user installs the extension and turns the
   * switch on, and then the platform they actually use does not move a single
   * conversation — to them that is indistinguishable from "it is broken".
   * The live leg supports 5 platforms and the backfill leg only 1; not saying so
   * leaves the user staring at a progress bar that never moves, guessing.
   *
   * 🔴 It is **derived from the same two tables** in lib/backfill/enumerate.ts:
   *    the platform list is not retyped here, so whoever fills in a platform
   *    changes this wording automatically instead of letting it drift.
   */
  coverage: string;
  /** Supplementary notes, possibly empty. */
  notes: string[];
  toggle: { label: string; checked: boolean; disabled: boolean };
  /** 🔴 C20 · The "got it / clear the failure list" button. Hidden when there are no failures. */
  clearFailures: { label: string; visible: boolean };
  /**
   * 🔴 C33 · The "start backfilling this platform" button. **Only appears when
   * there IS a usable channel and there are NO targets yet.**
   *  · targets exist ⇒ not shown: its job is already being done, and a permanent
   *    button is just noise;
   *  · no channel ⇒ not shown: pressing it cannot register "a platform that is
   *    live right now", so it would be a lie.
   */
  startBackfill: { label: string; visible: boolean };
  /** The popup's language selector: its label, its choices and the current value. */
  locale: { label: string; options: LocaleOption[]; value: UiLocale };
}

/** The switch row's fixed wording. The switch only means "the user agreed", never "it is running". */
export function toggleLabel(): string {
  return t('popup.toggle.label');
}

/** The clear button's fixed wording. "Got it" rather than "retry" — pressing it re-fetches nothing. */
export function clearFailuresLabel(): string {
  return t('popup.clearFailures.label');
}

/** An empty list. A constant for models, so nobody hand-assembles one. */
export const NO_FAILURES: FailureSummary = { entries: [], dropped: 0 };

/**
 * 🔴 C33 · The fixed wording for that button.
 * "Start backfilling **this** platform" — *this* means the platform the live
 * channel currently belongs to, not "all platforms" and not "all your accounts".
 * Pressing it does exactly one thing: record it as a backfill target.
 */
export function startBackfillLabel(): string {
  return t('popup.startBackfill.label');
}

/**
 * 🔴 The selector's choices, each labelled in its own language. `Auto` follows
 * the browser; the other two are the two shipped catalogs. The labels come
 * from the catalog rather than from literals here so that they read the same no
 * matter which language is active — a user who switched to English by mistake
 * must still be able to find their own language in the list.
 */
export function localeOptions(): LocaleOption[] {
  return [
    { value: 'auto', label: t('popup.locale.auto') },
    { value: 'en', label: t('popup.locale.english') },
    { value: 'zh_CN', label: t('popup.locale.chinese') },
  ];
}

/**
 * 🔴 The criterion for the button appearing, in **exactly one place** (the popup
 * and the tests share it; nobody writes their own). A usable channel AND no
 * backfill targets. Both are required.
 */
export function canStartBackfillHere(model: PopupModel): boolean {
  if (!model.liveTarget) return false;
  // An omitted targetCount ⇒ "we do not know whether there are any" ⇒ treat as
  // there being some ⇒ do not show. Better one button too few.
  return model.targetCount === 0;
}

/**
 * The delivery-channel line.
 * 🔴 Three values, none of which may be confused with another:
 *  · asked last time, host answered ⇒ report stage / machine / version faithfully;
 *  · asked last time, host did not answer ⇒ a named reason + the fix command
 *    (using the stage we knew last);
 *  · never asked ⇒ say "never asked" as-is, never guess a "probably fine".
 */
export function channelLine(model: PopupModel): string {
  const status = model.nativeHost;
  if (!status) return ui.channelNoCheck();
  return status.ok ? ui.channelConnected(status) : ui.channelDisconnected(status);
}

/** The outbox line. Empty and not full ⇒ null (then "nothing to send" is the whole truth and need not take a line). */
export function outboxLine(model: PopupModel): string | null {
  const box = model.outbox;
  if (box === undefined) return null;
  if (box === null) return ui.outboxUnreadable();
  if (box.pending === 0 && box.rejected === 0 && !box.full) return null;
  return ui.outboxLine(box);
}

/** The backfill-paused line. Not paused ⇒ null. */
export function pauseLine(model: PopupModel): string | null {
  const pause = model.hostPause;
  if (!pause) return null;
  return ui.backfillPaused(pause.at, pause.reason, pause.detail);
}

export function exportLine(model: PopupModel): string {
  const rec = model.lastExport;
  return rec ? ui.exportNote(rec) : ui.exportNoHistory();
}

export function renderPopup(model: PopupModel): PopupView {
  const status = statusLine(model);
  const channel = channelLine(model);
  const running = runningLine(model);
  const missing = missingLine(model) || null;
  const progress = progressLine(model);
  const hasFailures = model.failures.entries.length > 0 || model.failures.dropped > 0;
  const box = model.outbox ?? null;
  const hasUndelivered = box !== null && (box.pending > 0 || box.rejected > 0);

  return {
    status,
    channel,
    outbox: outboxLine(model),
    pause: pauseLine(model),
    // "Export undelivered captures": appears only when something has not been
    // delivered — an empty button is pure noise.
    exportFile: { label: ui.exportButtonLabel(), visible: hasUndelivered },
    lastExport: exportLine(model),
    failures: hasFailures ? failuresLine(model.failures) : null,
    running,
    missing,
    progress,
    coverage: coverageLine(),
    notes: notesFor(model),
    clearFailures: { label: clearFailuresLabel(), visible: hasFailures },
    startBackfill: { label: startBackfillLabel(), visible: canStartBackfillHere(model) },
    toggle: {
      label: toggleLabel(),
      checked: model.enabled,
      // 🔴 The switch stays operable while paused: the pause is a problem with
      //    the delivery exit, not "the user may not change their mind". It is
      //    disabled only when storage is unavailable, because then a flip cannot
      //    be saved — and the missing line says why.
      disabled: model.block === 'no-store',
    },
    locale: {
      label: t('popup.locale.label'),
      options: localeOptions(),
      value: currentUiLocale(),
    },
  };
}

function statusLine(model: PopupModel): string {
  if (model.block === 'host-paused') return t('popup.status.hostPaused');
  if (model.enabled) return t('popup.status.on');
  return t('popup.status.off');
}

/**
 * 🔴 The most important function in this file.
 * Every branch must let the user see "running / not running", and there must be
 * **no vague third answer**.
 */
function runningLine(model: PopupModel): string {
  switch (model.block) {
    case 'no-store':
      return t('popup.running.noStore');
    case 'disabled':
      return t('popup.running.disabled');
    case 'host-paused':
      // 🔴 W2 · This replaced C12's download-paused: there is now exactly one
      //    reason to pause — this machine's host is unreachable. Not one debt
      //    was moved, and it carries on from the same item once the host answers.
      return t('popup.running.hostPaused');
    case 'no-targets':
      // 🔴 C30 · This and 'no-http-port' are **two different things** and must
      //    be two different sentences: the channel may be perfectly connected
      //    (a platform page is open), but we do not even know "which account,
      //    which platform to start from" — the registry is empty.
      // 🔴 C32 · The last half-sentence is deliberate: this line only says "not
      //    running", and whoever finishes reading it asks "so what do I do" —
      //    the answer is on the very next line, so it points them there rather
      //    than letting them think this is all there is.
      return t('popup.running.noTargets');
    case 'no-http-port':
      // 🔴 This branch is the heart of C18, and C19 did not remove it: switch on
      //    but not one conversation being fetched has to be said as-is. Only the
      //    reason changed — it is now "no platform page is open".
      return t('popup.running.noHttpPort');
    case null:
      // 🔴 All four gates passed = switch on + storage present + not paused +
      //    **there really is a live, logged-in platform tab to fetch through
      //    right now**. The alarm is already running too (created when the
      //    switch was turned on). Only at this point may "archiving" be said.
      return t('popup.running.active', {
        minutes: BACKFILL_ALARM_PERIOD_MINUTES,
        maxPerDay: DEFAULT_DETAIL_PACE.maxPerDay,
        minIntervalSeconds: Math.round(DEFAULT_DETAIL_PACE.minIntervalMs / 1000),
      });
  }
}

function missingLine(model: PopupModel): string {
  switch (model.block) {
    case 'no-store':
      return t('popup.missing.noStore');
    case 'no-http-port':
      // 🔴 C19 changed the **reason** this wording gives, because the reason
      //    really changed: there is production injection for the port now, but
      //    it has to borrow an open, logged-in platform page. No page open means
      //    no channel.
      return t('popup.missing.noHttpPort');
    case 'no-targets':
      // 🔴 C32 · This one has to say the whole thing. C30 already got "not
      //    running" right, but the user's next question is "then what do I do to
      //    make it start?" — not answering it makes an honest sentence
      //    indistinguishable from "it is broken".
      //
      //    None of the three may be missing, and tests/c32-coldstart.test.ts
      //    pins each one:
      //      1. THE ACTION — "please wait" does not count;
      //      2. WHY — this is not a system limitation, it is a privacy promise:
      //         we do not guess your account;
      //      3. NO PROMISE WE CANNOT KEEP — we have no rate model, so never
      //         "it will start within a few minutes".
      //
      //    🔴 This is **only wording**. A registration still requires a real
      //    capture first; that is deliberate in alarm.ts:80-87 (when the alarm
      //    wakes the SW is brand new, with no tab and no account, and the only
      //    information that does not have to be invented is the one the live leg
      //    is holding), and nothing here changes it.
      //
      //    🔴 C33 · There is **one more route** now: the user can press the button
      //    and say outright "backfill this platform". So when the button is
      //    present it has to be mentioned — a button sitting on screen while the
      //    text only says "you have to send a message first" would be the UI
      //    arguing with itself. With no button (no channel) that sentence does
      //    not appear at all, so it never points at something that is not there.
      return (canStartBackfillHere(model)
        ? t('popup.missing.noTargets.buttonHint', { button: startBackfillLabel() }) + '\n'
        : '')
        + t('popup.missing.noTargets.body');
    case 'host-paused':
      return t('popup.missing.hostPaused');
    case 'disabled':
    case null:
      return '';
  }
}

/**
 * 🔴 C20 · The failure-list line. Three things must all be said:
 *   1. **how many were not stored** (the number comes from real entries, never an estimate);
 *   2. **no automatic retry** (that is a product decision the user has to know,
 *      or they will assume waiting fixes it);
 *   3. how many earlier entries the cap dropped (say it when it is non-zero —
 *      🔴 never a silent truncation).
 * 🔴 This line **does not guess a cause** (the C12 rule) — the specific reason
 *    codes are listed one by one in the notes below, and each states only the
 *    fact we observed.
 */
function failuresLine(summary: FailureSummary): string {
  const n = summary.entries.length;
  if (summary.dropped > 0) {
    return t('popup.failures.headDropped', { count: n, max: MAX_FAILURES, dropped: summary.dropped });
  }
  return t('popup.failures.head', { count: n });
}

/** Timestamp → a local-time string. A non-finite input is reported as unknown, never invented. */
function stampOf(at: number): string {
  if (!Number.isFinite(at)) return t('common.unknownTimeShort');
  try {
    return new Date(at).toISOString().replace('T', ' ').slice(0, 19) + ' UTC';
  } catch {
    return t('common.unknownTimeShort');
  }
}

function failureNote(summary: FailureSummary): string {
  const lines = [
    t('popup.failureNote.title'),
    ...summary.entries.map(
      (e) => t('popup.failureNote.row', {
        platform: e.platform,
        shortId: e.shortId,
        reason: describeFailureReason(e.reason),
        at: stampOf(e.at),
      }),
    ),
    '',
    t('popup.failureNote.retry'),
    t('popup.failureNote.ledger'),
    t('popup.failureNote.clear'),
  ];
  return lines.join('\n');
}

/**
 * 🔴 C30 · "Did the alarm ever wake up, and what did that tick do?"
 *
 * Why this section has to exist: the status line describes the gates **right
 * now**, while the user's real question is "what has it been doing for the last
 * few hours". Before C30 that answer existed nowhere — the alarm woke every 5
 * minutes and silently skipped, reporting no error and leaving no trace.
 * 🔴 This only restates the record in storage; it infers nothing.
 */
function lastTickNote(rec: BackfillTickRecord | null): string | null {
  if (!rec) return t('popup.lastTick.none');
  const when = stampOf(rec.at);
  if (rec.ran) return t('popup.lastTick.ran', { when, targets: rec.targets });
  return t('popup.lastTick.skipped', {
    when,
    reason: describeTickReason(rec.reason),
    targets: rec.targets,
  });
}

/** Named outcome → one plain sentence. 🔴 Each one must read differently, or the naming is pointless. */
export function describeTickReason(reason: string): string {
  switch (reason) {
    case 'no-targets':
      return t('tick.reason.noTargets');
    case 'no-http-port':
      return t('tick.reason.noHttpPort');
    case 'disabled':
      return t('tick.reason.disabled');
    case 'no-store':
      return t('tick.reason.noStore');
    case 'host-paused':
      return t('tick.reason.hostPaused');
    case 'already-running':
      return t('tick.reason.alreadyRunning');
    case 'ran':
      return t('tick.reason.ran');
    default:
      // 🔴 An unrecognised outcome is reported verbatim; never swallow one we have not seen.
      return t('tick.reason.unknown', { reason });
  }
}

function progressLine(model: PopupModel): string {
  // 🔴 The single source of progress wording. This file does no percentage arithmetic.
  if (!model.state) return t('popup.progress.notStarted');
  // 🔴 W13: `now` is passed through so the transient-retry prefix's "about M
  //    minutes" and the note below can never disagree — both are computed from the
  //    same instant, and both are assertable in tests.
  return t('popup.progress.line', { progress: formatProgress(model.state, model.now ?? Date.now()) });
}

/**
 * 🔴 C22 · The backfill-coverage line. Three things must all be said:
 *   1. **which platforms can have their history backfilled** (the list comes from BACKFILL_PLANS, not retyped);
 *   2. **which cannot, for now** (the list comes from BACKFILL_UNSUPPORTED);
 *   3. **cannot ≠ broken ≠ no history** — this must be written into the wording,
 *      because "not moving" reads as "broken" by default.
 * 🔴 This line mentions no progress, no time estimate, and contains no percent sign.
 */
export function coverageLine(): string {
  const yes = BACKFILL_SUPPORTED_PLATFORMS.join(', ');
  const no = BACKFILL_UNSUPPORTED_PLATFORMS.join(', ');
  if (no.length === 0) return t('popup.coverage.all', { yes });
  return t('popup.coverage.partial', { yes, no });
}

/** What each platform is missing. Goes into the notes for a user who wants a closer look — the main line gives only the conclusion. */
function coverageNote(): string {
  const lines = [t('popup.coverage.noteTitle')];
  // 🔴 C26 · Platforms that "can list conversations but not fetch bodies yet"
  //    belong here too. They are closer to working, but the user-visible outcome
  //    is still **not one conversation backfilled** — so they belong in this
  //    section and must not be written as if the platform were already supported.
  for (const half of BACKFILL_PARTIAL) {
    lines.push(`· ${t(half.userNoteKey)}`);
  }
  for (const gap of BACKFILL_UNSUPPORTED) {
    lines.push(`· ${t(gap.userNoteKey)}`);
  }
  lines.push('');
  lines.push(t('popup.coverage.noteWhy'));
  return lines.join('\n');
}

function notesFor(model: PopupModel): string[] {
  const notes: string[] = [];
  // 🔴 Failure details come before every other note. If something was lost, say that first.
  if (model.failures.entries.length > 0) notes.push(failureNote(model.failures));

  const tickNote = lastTickNote(model.lastTick ?? null);
  if (tickNote) notes.push(tickNote);

  if (model.target) {
    notes.push(t('popup.notes.target', { platform: model.target.platform, scope: model.target.scope }));
  } else if (model.state === null) {
    notes.push(t('popup.notes.noTarget'));
  }

  if (!model.enabled) {
    notes.push(t('popup.notes.whatOpening', {
      minutes: BACKFILL_ALARM_PERIOD_MINUTES,
      maxPerDay: DEFAULT_DETAIL_PACE.maxPerDay,
    }));
  }

  if (model.state?.halted) {
    // 🔴 C22 · 'unsupported-platform' is not "it broke and stopped", it is "we
    //    have not written this platform yet". Both must leave a trace, and they
    //    must never be the same sentence.
    if (model.state.halted.reason === 'unsupported-platform') {
      notes.push(t('popup.notes.halted.unsupportedPlatform', {
        platform: model.state.platform,
        detail: model.state.halted.detail,
      }));
    } else if (haltClassOf(model.state.halted.reason) === 'transient') {
      // 🔴 W13 · This is the sentence that did not exist, and its absence is why a
      //    real account sat at 0 archived for over an hour. A transient stop must
      //    NOT read like the `other` fallback below ("this leg has stopped"): it has
      //    not stopped. It is waiting out a backoff, it will come back by itself,
      //    and not one debt was written off while it waited. So the note says all
      //    four of those things, plus the one number the user actually wants —
      //    when the next attempt is.
      notes.push(t('popup.notes.halted.waitingRetry', {
        reason: model.state.halted.reason,
        attempts: model.state.halted.attempts ?? 1,
        minutes: retryMinutesLeft(model.state.halted, model.now ?? Date.now()),
        detail: model.state.halted.detail,
      }));
    } else if (model.state.halted.reason === 'detail-unsupported') {
      // 🔴 C26 · This one must **not** say "stopped before issuing any request" —
      //    the list request really went out and conversations really were listed.
      //    Stopping half way and never starting are two different things to a user.
      notes.push(t('popup.notes.halted.detailUnsupported', {
        platform: model.state.platform,
        pending: model.state.pending.length,
        detail: model.state.halted.detail,
      }));
    } else {
      notes.push(t('popup.notes.halted.other', {
        reason: model.state.halted.reason,
        detail: model.state.halted.detail,
      }));
    }
  }

  // 🔴 The coverage note comes last: it is a long-term fact, not the current state.
  if (BACKFILL_UNSUPPORTED.length > 0 || BACKFILL_PARTIAL.length > 0) notes.push(coverageNote());
  return notes;
}

/** Flatten a view into plain text — used both by test assertions and by "paste the whole wording" reports. */
export function popupText(view: PopupView): string {
  const lines = [view.status];
  if (view.channel) lines.push(view.channel);
  // 🔴 W2: the pause line follows the channel line — it is the next consequence
  //    of the channel being broken, and the two sentences must be read together.
  if (view.pause) lines.push(view.pause);
  if (view.outbox) lines.push(view.outbox);
  lines.push(view.lastExport);
  if (view.failures) lines.push(view.failures);
  lines.push(view.running);
  if (view.missing) lines.push(view.missing);
  // 🔴 C33: a button is a real, pressable thing on screen, so it has to be
  //    visible in the flattened text too — otherwise "did it appear" cannot be
  //    asserted at all.
  if (view.startBackfill.visible) lines.push(t('popup.buttonTag', { label: view.startBackfill.label }));
  if (view.exportFile.visible) lines.push(t('popup.buttonTag', { label: view.exportFile.label }));
  lines.push(view.progress);
  lines.push(view.coverage);
  for (const n of view.notes) lines.push('', n);
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// Pick one debt set out of the full storage.local snapshot.
// The popup does not know which account the user is currently on (that
// information is carried by live-leg messages and nowhere else), so it can only
// list the sets that exist. It shows the one with the most archived — stable,
// explainable, and independent of any insertion order.
// ---------------------------------------------------------------------------

const STATE_KEY_PREFIX = `cs_backfill_v${BACKFILL_STATE_VERSION}:`;

function looksLikeState(value: unknown): value is BackfillState {
  if (!value || typeof value !== 'object') return false;
  const s = value as Partial<BackfillState>;
  return s.v === BACKFILL_STATE_VERSION
    && typeof s.platform === 'string'
    && typeof s.scope === 'string'
    && Array.isArray(s.pending)
    && Array.isArray(s.archived);
}

export function pickBackfillState(snapshot: Record<string, unknown> | null): BackfillState | null {
  if (!snapshot) return null;
  let best: BackfillState | null = null;
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(STATE_KEY_PREFIX)) continue;
    if (!looksLikeState(value)) continue;
    // The key carries platform/scope and so does the value; only accept the two
    // agreeing, so a mismatched progress set can never be displayed.
    if (stateKey(value.platform, value.scope) !== key) continue;
    if (!best || value.archived.length > best.archived.length) best = value;
  }
  return best;
}

/** Every valid debt set in the snapshot (those whose key and value agree). Failure aggregation and clearing walk it. */
export function backfillStateEntries(
  snapshot: Record<string, unknown> | null,
): Array<{ key: string; state: BackfillState }> {
  if (!snapshot) return [];
  const out: Array<{ key: string; state: BackfillState }> = [];
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(STATE_KEY_PREFIX)) continue;
    if (!looksLikeState(value)) continue;
    if (stateKey(value.platform, value.scope) !== key) continue;
    out.push({ key, state: value });
  }
  return out;
}

/**
 * 🔴 C20 · Aggregate the failure lists of **every** platform/account into one.
 * The progress row shows only one set; if failures only looked at that one, what
 * was lost under another account would vanish from the UI — which is exactly
 * "showing a state with failures as if everything were fine".
 * Sort: newest first (the most recent is the most useful for diagnosis).
 */
export function collectFailures(snapshot: Record<string, unknown> | null): FailureSummary {
  const entries: FailureEntry[] = [];
  let dropped = 0;
  for (const { state } of backfillStateEntries(snapshot)) {
    entries.push(...failuresOf(state));
    dropped += droppedOf(state);
  }
  entries.sort((a, b) => b.at - a.at);
  return { entries, dropped };
}
