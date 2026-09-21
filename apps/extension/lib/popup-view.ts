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
import {
  parseDashboardUrl,
  type DashboardResult,
  type StageSummary,
  type TimeState,
  type UndeliveredReason,
} from './native-host';
import { formatProgress, progressOfHeader, retryMinutesLeft } from './backfill/progress';
import {
  describeFailureReason,
  droppedOf,
  failuresOf,
  MAX_FAILURES,
  type FailureEntry,
} from './backfill/failures';
import {
  BACKFILL_TICK_DELAY_MAX_MINUTES,
  BACKFILL_TICK_DELAY_MIN_MINUTES,
  type BackfillTickRecord,
  type LegacyMigration,
} from './backfill/alarm';
import {
  BACKFILL_PARTIAL,
  BACKFILL_SUPPORTED_PLATFORMS,
  BACKFILL_UNSUPPORTED,
  BACKFILL_UNSUPPORTED_PLATFORMS,
} from './backfill/enumerate';
import { DAILY_CAP_MAX, DEFAULT_DETAIL_PACE } from './backfill/pace';
import type { TickBlockReason } from './backfill/schedule';
import {
  BACKFILL_STATE_VERSION,
  haltClassOf,
  isHeader,
  stateKey,
  type BackfillHeader,
} from './backfill/types';
import type { HostPauseRecord, HostStatusRecord } from './host-status';
import {
  HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN,
  HOOK_DECLINE_UNREADABLE_MESSAGE,
  isHookDecline,
  type HookDecline,
  type HookDeclineRecord,
  type HookStatusRecord,
} from './hook-status';
import {
  HOOK_REASON_DID_NOT_RUN,
  HOOK_REASON_DID_NOT_TAKE,
  HOOK_REASON_WAS_REPLACED,
  type HookObservation,
} from './contract';
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
  state: BackfillHeader | null;
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
   * 🔴 W36b · **What the popup's own state load did with the pre-W18 records.**
   *
   * The popup is a place a user's storage layout gets loaded, and until now it
   * was the one place that showed the old layout's absence and moved nothing:
   * a user who opened it in the 5-10 minutes before the next alarm tick, or with
   * the switch off so no alarm fires at all, saw "not started yet" over a
   * `cs_backfill_v1:*` record that was sitting right there. The scan now runs
   * here too, on the same function the tick preflight uses.
   *
   * Omitted ⇒ this call site does not do the scan (the optional-field pattern the
   * other fields here follow, so no existing call site changes a character).
   */
  legacyMigration?: LegacyMigration | null;
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
  /**
   * 🔴 W30 · The host's answer to §6.4's `summary`, as the popup received it.
   *
   * Three states, and the difference between them is the whole point:
   * an *answer* (which may still contain unknown parts, each with its reason), a
   * *failure* (the host missing, older than this extension, or refusing), and
   * *unasked*. Omitted ⇒ unasked, the same optional-field pattern as above, so
   * no existing call site changes a character.
   */
  summary?: SummaryState;
  /**
   * 🔴 W43 · **Every origin whose top frame told us its capture hook is not
   *    working** (`lib/hook-status.ts`), newest first.
   *
   * This is the field that turns "the stage stayed empty" from an ambiguity into
   * a fact: before W43 a page where nothing of ours ran produced exactly what a
   * page where the user opened no conversation produces, and no surface of the
   * extension could tell them apart.
   *
   * Omitted ⇒ this load did not look (the same optional-field pattern as the
   * fields above). An unreadable storage snapshot reads as an empty list here, the
   * same rule `failures` follows: at that point nothing is known about any origin,
   * and the alternative — a note claiming a failure that may not exist — would be
   * worse than the silence it replaces.
   */
  hookStatus?: HookStatusRecord[];
  /**
   * 🔴 W47 · **A report a page sent that background received and did not record,
   *    and why** (`lib/hook-status.ts`'s `HookDeclineRecord`).
   *
   * The other half of the field above, and it exists because the two states were
   * one state: a page whose observation was *declined* left exactly the same thing
   * behind as a page that never sent one — nothing. On the machine W47 is about, a
   * page reported `hook-was-replaced` every 5 seconds and every one of those
   * reports was refused by the origin check; what the user could look at was a
   * blank, so the failure was chased in fourteen wrong directions.
   *
   * Omitted or null ⇒ nothing has been declined (or this build has not looked) —
   * and like `hookStatus`, `null` here must not be worded as anything else.
   */
  hookDecline?: HookDeclineRecord | null;
}

/**
 * 🔴 W30 · What the popup knows about the stage summary.
 *
 * `failed` is not "zero sessions": it is "no answer", and the two must never
 * render as the same sentence. `reason` is the wire-level reason code, kept so
 * the wording can distinguish "the host is not installed" from "the host
 * refused" without re-parsing a sentence.
 */
export type SummaryState =
  | { kind: 'answer'; summary: StageSummary }
  | {
      kind: 'failed';
      reason: UndeliveredReason;
      detail?: string;
      /** True when the installed host predates §6.4 (a `bad-request` nack). */
      olderHost: boolean;
    }
  | { kind: 'unasked' };

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
  /**
   * 🔴 W30 · The one summary line: what the host counts in the stage (last
   * `window_hours` and in total, split by harness) and when the last successful
   * push was. An unknown part says so in the line; the reason is in the notes.
   */
  summary: string;
  /**
   * 🔴 W30 · The "Open dashboard" button. Disabled — with the reason stated —
   * when there is no usable answer from the host, because a button that cannot
   * do anything must not look like one that can.
   */
  dashboard: { label: string; enabled: boolean; reason: string | null };
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
    summary: summaryLine(model),
    dashboard: dashboardButton(model),
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
      // 🔴 W16 · The rate is no longer one number, so it can no longer be
      //    stated as one number. The popup says the *range* the tick gap is
      //    drawn from and the ceiling the daily cap is drawn under, and never a
      //    figure the leg does not actually honour: the old text promised
      //    "every 5 minutes", which was true then and would now be a lie.
      return t('popup.running.active', {
        minMinutes: BACKFILL_TICK_DELAY_MIN_MINUTES,
        maxMinutes: BACKFILL_TICK_DELAY_MAX_MINUTES,
        maxPerDay: DAILY_CAP_MAX,
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
  const head = rec.ran
    ? t('popup.lastTick.ran', { when, targets: rec.targets })
    : t('popup.lastTick.skipped', {
      when,
      reason: describeTickReason(rec.reason),
      targets: rec.targets,
    });
  // 🔴 W36b · **"It ran" and "it got anywhere" are different sentences.** A run
  //    that halts on a record it cannot read returns a report like any other, so
  //    `ran: true` used to be the whole story the popup told — and the one thing
  //    the user needed (this leg is stopped, here is why) was in the trace and
  //    printed nowhere. The trace now carries the run's own halt, and this is
  //    where it is read back out.
  if (!rec.halted) return head;
  // 🔴 W45 · One reason reads badly in the generic form, and it is the one that
  //    means "rows are provably gone". `popup.lastTick.halted` says "the tick
  //    stopped before it could finish", which is true but leaves out the only two
  //    facts a user needs: that this was a *loss* rather than a pause, and that the
  //    scope was reset so the list is read again. It gets its own sentence for the
  //    same reason `org-ambiguous` did — a named reason whose wording is the
  //    `other` fallback's is not really named.
  if (rec.halted === 'ledger-mismatch') {
    return `${head}\n${t('popup.lastTick.ledgerMismatch', {
      detail: rec.detail ?? t('common.unknownShort'),
    })}`;
  }
  return `${head}\n${t('popup.lastTick.halted', {
    reason: rec.halted,
    detail: rec.detail ?? t('common.unknownShort'),
  })}`;
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
  return t('popup.progress.line', { progress: formatProgress(progressOfHeader(model.state), model.now ?? Date.now()) });
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

// ---------------------------------------------------------------------------
// 🔴 W30 · The stage summary and the dashboard button
// ---------------------------------------------------------------------------

/** How many harness names the summary line shows before it says "and N more". */
export const MAX_SUMMARY_HARNESSES = 5;

/**
 * A duration in the largest unit that is still true.
 *
 * 🔴 It never says "0 minutes": under 90 seconds the honest sentence is "just
 *    now". Nothing here estimates a rate or a time-to-completion; this is an
 *    age of something that already happened.
 */
export function agoText(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return t('common.unknownTimeShort');
  if (seconds < 90) return t('popup.summary.agoJustNow');
  const minutes = Math.round(seconds / 60);
  if (minutes < 90) return t('popup.summary.agoMinutes', { count: minutes });
  const hours = Math.round(minutes / 60);
  if (hours < 48) return t('popup.summary.agoHours', { count: hours });
  return t('popup.summary.agoDays', { count: Math.round(hours / 24) });
}

/**
 * When the last successful push was.
 *
 * 🔴 Three outcomes, and they stay three: an age, "unknown" (with the reason in
 *    the notes), or — when the record's timestamp is in the future — the
 *    timestamp itself, because "in 3 hours" is not an age and pretending
 *    otherwise would hide a disagreement between the clock and the record.
 */
export function pushText(push: TimeState, now: number): string {
  if (push.kind === 'unknown') return t('popup.summary.pushUnknown');
  const seconds = Math.round(now / 1000) - push.unix;
  if (seconds < 0) {
    return t('popup.summary.pushFuture', { at: ui.stamp(push.unix * 1000) });
  }
  return t('popup.summary.pushAgo', { ago: agoText(seconds) });
}

/**
 * The harness split of the window's sessions: "deepseek 3, claude-code 9".
 *
 * 🔴 These are the harness ids **verbatim from the host**, never a prettier name
 *    this file invented — the popup has no authority to rename what it was told.
 *    A bucket whose name was unusable is labelled, not dropped.
 * 🔴 A bucket that is unknown is not listed here; the line says the window
 *    itself is unknown and the note says why.
 */
export function harnessBreakdown(summary: StageSummary): string {
  if (summary.last24h.kind !== 'known') return '';
  const rows: Array<{ label: string; count: number }> = [];
  for (const row of summary.byHarness) {
    if (row.last_24h.kind !== 'known' || row.last_24h.count === 0) continue;
    rows.push({ label: row.harness ?? t('popup.summary.noHarness'), count: row.last_24h.count });
  }
  rows.sort((a, b) => b.count - a.count || a.label.localeCompare(b.label));
  const shown = rows.slice(0, MAX_SUMMARY_HARNESSES);
  const list = shown
    .map((row) => t('popup.summary.harnessEntry', { harness: row.label, count: row.count }))
    .join(', ');
  if (rows.length > shown.length) {
    return t('popup.summary.harnessMore', { list, rest: rows.length - shown.length });
  }
  return list;
}

/** The summary line for the "no usable answer" states: never a zero, always the reason. */
function summaryFailureLine(state: Exclude<SummaryState, { kind: 'answer' }>): string {
  if (state.kind === 'unasked') return t('popup.summary.unasked');
  if (state.olderHost) return t('popup.summary.olderHost');
  if (state.reason === 'no-runtime-api' || state.reason === 'send-failed') {
    return t('popup.summary.hostMissing');
  }
  return t('popup.summary.unavailable', { reason: state.detail || state.reason });
}

/**
 * 🔴 W30 · The one summary line.
 *
 * Every part is either a number the host measured or the word for unknown — no
 * part of it is ever filled in with a plausible default, and a part the host
 * could not read never becomes `0`.
 */
export function summaryLine(model: PopupModel): string {
  const state = model.summary;
  if (!state || state.kind !== 'answer') {
    return summaryFailureLine(state ?? { kind: 'unasked' });
  }
  const summary = state.summary;
  // 🔴 The parentheses around the split live in the catalog, in
  //    `popup.summary.recentNamed` — an unknown or empty split must not leave an
  //    empty `()` on screen, and assembling punctuation here would be a second
  //    place the sentence is decided.
  const breakdown = harnessBreakdown(summary);
  const recent =
    summary.last24h.kind !== 'known'
      ? t('popup.summary.recentUnknown', { hours: summary.windowHours })
      : breakdown.length === 0
        ? t('popup.summary.recent', {
            hours: summary.windowHours,
            count: summary.last24h.count,
          })
        : t('popup.summary.recentNamed', {
            hours: summary.windowHours,
            count: summary.last24h.count,
            list: breakdown,
          });
  const total =
    summary.total.kind === 'known'
      ? t('popup.summary.totalNamed', { count: summary.total.count })
      : t('popup.summary.totalUnknown');
  return t('popup.summary.line', {
    recent,
    total,
    push: pushText(summary.lastPush, model.now ?? Date.now()),
  });
}

/** The button's fixed wording. */
export function dashboardLabel(): string {
  return t('popup.dashboard.label');
}

/**
 * 🔴 W30 · Whether the button can be pressed, and — when it cannot — why.
 *
 * The condition is deliberately narrow: the host must have *answered* §6.4. An
 * answer whose counts are partly unknown still means the host is reachable, so
 * the button stays usable; only "no answer" disables it. A disabled button
 * always carries its reason, so it is never a dead control with no explanation.
 */
export function dashboardButton(model: PopupModel): {
  label: string;
  enabled: boolean;
  reason: string | null;
} {
  const label = dashboardLabel();
  const state = model.summary;
  if (!state || state.kind === 'unasked') {
    return { label, enabled: false, reason: t('popup.dashboard.reason.noAnswer') };
  }
  if (state.kind === 'answer') {
    return { label, enabled: true, reason: null };
  }
  if (state.olderHost) {
    return { label, enabled: false, reason: t('popup.dashboard.reason.olderHost') };
  }
  if (state.reason === 'no-runtime-api' || state.reason === 'send-failed') {
    return { label, enabled: false, reason: t('popup.dashboard.reason.hostMissing') };
  }
  return {
    label,
    enabled: false,
    reason: t('popup.dashboard.reason.failed', { detail: state.detail || state.reason }),
  };
}

/** The reasons behind the summary's unknown parts. Empty when there are none. */
function summaryNotes(model: PopupModel): string[] {
  const state = model.summary;
  if (!state || state.kind === 'unasked') return [];
  if (state.kind === 'failed') {
    return [
      state.olderHost
        ? t('popup.summary.olderHostNote')
        : t('popup.summary.failedNote', { detail: state.detail || state.reason }),
    ];
  }
  const summary = state.summary;
  const notes: string[] = [];
  // 🔴 One note per unknown part, each with the host's own reason. A response
  //    that is complete produces none — there is nothing to explain.
  if (summary.total.kind === 'unknown') {
    notes.push(t('popup.summary.noteTotal', { why: summary.total.why }));
  }
  if (summary.last24h.kind === 'unknown') {
    notes.push(t('popup.summary.noteRecent', { why: summary.last24h.why }));
  }
  if (summary.lastPush.kind === 'unknown') {
    notes.push(t('popup.summary.notePush', { why: summary.lastPush.why }));
  }
  return notes;
}

/**
 * 🔴 W30 · Ask the host for a dashboard and open **exactly** what it answered.
 *
 * The rules this function exists to hold:
 *  1. nothing is opened unless the host answered `ok` — a refusal, a timeout, a
 *     missing host or a malformed response opens nothing at all;
 *  2. the URL is checked with [`parseDashboardUrl`] before it is used, so a
 *     response that is not a loopback URL with a token opens nothing;
 *  3. the opener is called at most once, and only with the host's own URL.
 *
 * `ask` and `open` are parameters so this runs under `node` in the test suite:
 * the popup's own wiring cannot be exercised there, and these three rules are
 * exactly what must not depend on the popup being open in a real browser.
 */
export async function openDashboardTab(
  ask: () => Promise<DashboardResult>,
  // `Promise<unknown>` rather than `Promise<void>`: the real opener is
  // `browser.tabs.create`, which resolves with the created tab and has no
  // business being told to throw that away.
  open: (url: string) => void | Promise<unknown>,
): Promise<{ url: string | null; message: string }> {
  let result: DashboardResult;
  try {
    result = await ask();
  } catch (err) {
    return {
      url: null,
      message: t('popup.dashboard.failed', { detail: (err as Error)?.message ?? String(err) }),
    };
  }
  if (!result.ok) {
    if (result.olderHost) return { url: null, message: t('popup.dashboard.reason.olderHost') };
    return {
      url: null,
      message: t('popup.dashboard.failed', { detail: result.detail || result.reason }),
    };
  }
  const url = parseDashboardUrl(result.url);
  if (url === null) return { url: null, message: t('popup.dashboard.refusedUrl') };
  try {
    await open(url);
  } catch (err) {
    return {
      url: null,
      message: t('popup.dashboard.failed', { detail: (err as Error)?.message ?? String(err) }),
    };
  }
  return { url, message: t('popup.dashboard.opened') };
}

/**
 * 🔴 W36b · One sentence per outcome of the pre-W18 sweep, and they must not be
 * the same sentence. "Moved" is a completed repair; "refused" means the old
 * record is still there and still holding the user's ids, which is the fact the
 * first real-Chrome acceptance could not get at.
 */
function legacyMigrationNote(migration: LegacyMigration): string {
  if (migration.moved < migration.found) {
    return t('popup.notes.legacy.refused', {
      found: migration.found,
      moved: migration.moved,
      reason: migration.refusal?.reason ?? t('common.unknownShort'),
      detail: migration.refusal?.detail ?? t('common.unknownShort'),
    });
  }
  return t('popup.notes.legacy.moved', { count: migration.moved });
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

/**
 * 🔴 W43 · **One reason code → the sentence a person reads.**
 *
 * A total map over the closed set in `lib/contract.ts`, not a lookup with a
 * fallback: a reason this table does not name must fail `tsc` here rather than
 * reaching a user as its own raw code or, worse, as a sentence about a different
 * fact.
 */
const HOOK_REASON_NOTE_KEYS: Record<HookObservation, string> = {
  [HOOK_REASON_DID_NOT_RUN]: 'popup.notes.hook.reason.didNotRun',
  [HOOK_REASON_DID_NOT_TAKE]: 'popup.notes.hook.reason.didNotTake',
  [HOOK_REASON_WAS_REPLACED]: 'popup.notes.hook.reason.wasReplaced',
};

/**
 * 🔴 W43 · One origin's hook record, as a sentence.
 *
 * Every observation the record holds is named — they are different facts and the
 * one that matters to a reader (did captures happen and then stop, or did nothing
 * ever run?) differs between them — and the time is printed as the stamp it is,
 * because "the hook is not working" is only ever true **as of** that moment: a
 * top frame that verifies afterwards clears the record.
 */
function hookStatusNote(record: HookStatusRecord): string {
  const reasons = record.reasons
    .map((row) => t(HOOK_REASON_NOTE_KEYS[row.reason]))
    .join(' · ');
  return t('popup.notes.hook.notInstalled', {
    platform: record.platform,
    origin: record.origin,
    reasons,
    when: ui.stamp(record.at),
  });
}

/**
 * 🔴 W47 · **One report a page sent that background did not write down, and why.**
 *
 * The sentence has two jobs and the reason code decides which one it does:
 *
 *  · `not-a-platform-origin` names the origin, which is the only thing a person
 *    needs to open the tab that has the problem — and it says plainly that this is
 *    why nothing was recorded, so the user does not go looking for a record that
 *    was never going to exist;
 *  · `unreadable-message` names no origin, because a report this build could not
 *    read is one whose origin is exactly the part that did not check out. It says
 *    the report arrived and was not understood; inventing the rest would be the
 *    failure it exists to report.
 *
 * `count` is the length of the streak, and it is printed because that is the fact
 * that turns "one odd message" into "this is happening on a timer" — the measured
 * case was one report every 5 seconds over a page whose user could see none of it.
 * An unrecognised reason code prints verbatim rather than being rounded into one of
 * the two above (the same rule `capabilityWords` follows).
 */
function hookDeclineNote(record: HookDeclineRecord): string {
  const when = ui.stamp(record.at);
  const count = record.count;
  // 🔴 R47 · Two different unknowns, kept apart. A code this build does not know
  //    comes from a NEWER build and must print as itself (HookDeclineRecord.reason
  //    says why). But a member added to HOOK_DECLINES in THIS build with no
  //    sentence must not quietly fall through to that same fallback — so the known
  //    set is an exhaustive Record, the pattern HOOK_REASON_NOTE_KEYS already uses,
  //    and a new member fails to compile until someone writes its words.
  if (!isHookDecline(record.reason)) {
    return t('popup.notes.hook.declinedOther', { reason: record.reason, when, count });
  }
  const known: Record<HookDecline, () => string> = {
    [HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN]: () => t('popup.notes.hook.declinedOrigin', {
      origin: record.origin ?? t('common.unknownShort'),
      when,
      count,
    }),
    [HOOK_DECLINE_UNREADABLE_MESSAGE]: () => t('popup.notes.hook.declinedMessage', { when, count }),
  };
  return known[record.reason]();
}

/**
 * 🔴 W44 · **One capability value, in words.**
 *
 * The popup has to name both sides of an expiry — what the record said the build
 * could do, and what this build can do — and a reader who is told "judged against
 * list-only, now full" has been told nothing. So each value becomes the sentence a
 * person would use.
 *
 * 🔴 `unmarked` is a *kind of statement*, not a capability, and it gets words that
 *    say so: the record did not say. It is never rounded into 'none' — "this build
 *    could do nothing" and "this record never said what the build could do" are
 *    different facts, and a reader deciding whether to trust the note needs to know
 *    which one they are looking at.
 * 🔴 An unrecognised value is printed verbatim rather than defaulted: a value this
 *    build does not know is exactly the case where a guess would be invented.
 */
const CAPABILITY_WORDS: Record<string, string> = {
  none: 'popup.capability.none',
  'list-only': 'popup.capability.listOnly',
  full: 'popup.capability.full',
  unmarked: 'popup.capability.unmarked',
};

function capabilityWords(value: string): string {
  const key = CAPABILITY_WORDS[value];
  return key ? t(key) : t('popup.capability.other', { capability: value });
}

function notesFor(model: PopupModel): string[] {
  const notes: string[] = [];
  // 🔴 Failure details come before every other note. If something was lost, say that first.
  if (model.failures.entries.length > 0) notes.push(failureNote(model.failures));

  // 🔴 W43 · **Second, and before the tick note, because it outranks it.** A
  //    backfill that has not ticked yet is a wait; a page whose capture hook is
  //    not installed is live capture that will never produce anything, on a page
  //    the user is looking at right now. Saying "waiting" first would bury it.
  for (const record of model.hookStatus ?? []) notes.push(hookStatusNote(record));

  // 🔴 W47 · **And immediately after it, the page whose report was not recorded.**
  //    The two are neighbours on purpose: `hookStatusNote` says "a page told us
  //    its hook is not installed", and this one says "a page told us something and
  //    we did not write it down". Both are about a page the user is looking at,
  //    both outrank the backfill tick note, and until W47 the second one had no
  //    sentence anywhere — the report was received, refused, and forgotten, which
  //    read from outside exactly like a page that never reported anything.
  if (model.hookDecline) notes.push(hookDeclineNote(model.hookDecline));

  const tickNote = lastTickNote(model.lastTick ?? null);
  if (tickNote) notes.push(tickNote);

  // 🔴 W36b · The pre-W18 layout, said out loud at the moment this popup itself
  //    tried to move it. Before this, a user whose storage still held the old
  //    key saw only "Progress: not started yet" beside it — the one place the
  //    old layout was visible was the one place that did not mention it.
  if (model.legacyMigration && model.legacyMigration.found > 0) notes.push(legacyMigrationNote(model.legacyMigration));

  if (model.target) {
    notes.push(t('popup.notes.target', { platform: model.target.platform, scope: model.target.scope }));
  } else if (model.state === null) {
    notes.push(t('popup.notes.noTarget'));
  }

  if (!model.enabled) {
    notes.push(t('popup.notes.whatOpening', {
      minMinutes: BACKFILL_TICK_DELAY_MIN_MINUTES,
      maxMinutes: BACKFILL_TICK_DELAY_MAX_MINUTES,
      maxPerDay: DAILY_CAP_MAX,
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
    } else if (model.state.halted.reason === 'org-ambiguous' || model.state.halted.reason === 'org-unresolved') {
      // 🔴 W31c · The two organization halts. Each gets its own sentence, and
      //    neither may fall through to `other`: `other` prints the reason code and
      //    the technical detail, which is a description of the state rather than
      //    the one thing a user can do about it. Both of these have exactly one
      //    action, and it is a human action — which is why the leg stopped.
      notes.push(t(model.state.halted.reason === 'org-ambiguous'
        ? 'popup.notes.halted.orgAmbiguous'
        : 'popup.notes.halted.orgUnresolved'));
    } else if (model.state.halted.reason === 'detail-unsupported') {
      // 🔴 C26 · This one must **not** say "stopped before issuing any request" —
      //    the list request really went out and conversations really were listed.
      //    Stopping half way and never starting are two different things to a user.
      notes.push(t('popup.notes.halted.detailUnsupported', {
        platform: model.state.platform,
        pending: model.state.pendingCount,
        detail: model.state.halted.detail,
      }));
    } else {
      notes.push(t('popup.notes.halted.other', {
        reason: model.state.halted.reason,
        detail: model.state.halted.detail,
      }));
    }
  }

  // 🔴 W44 · **A stop that stopped applying, said out loud.**
  //
  //    When a stored capability-class halt expires, the engine clears it and the
  //    leg starts again — and a cleared record is silence. This note is what a user
  //    sees instead: which judgement stopped applying, what the record said the
  //    build could do, what this build can do, and when the leg started again. It
  //    reads the header (`haltExpired`), which survives the run that wrote it.
  //
  //    🔴 It is NOT the sentence for a stop still in force, and that is the point:
  //      `popup.notes.halted.unsupportedPlatform` says this platform's history
  //      cannot be backfilled yet, which is the opposite of what happened here.
  //      Printing one for the other would be the same defect wearing a different
  //      coat — a leg that starts again for no stated reason reads as a leg that
  //      fixed itself, which is not what happened either.
  if (model.state?.haltExpired) {
    notes.push(t('popup.notes.haltExpired', {
      reason: model.state.haltExpired.reason,
      when: stampOf(model.state.haltExpired.recordedAt),
      judgedAgainst: capabilityWords(model.state.haltExpired.judgedAgainst),
      capability: capabilityWords(model.state.haltExpired.capability),
      cleared: stampOf(model.state.haltExpired.clearedAt),
    }));
  }

  // 🔴 W45 · **The durable half of the `ledger-mismatch` refusal.**
  //
  //    The refusal itself is a run's halt record: it is right there in the tick
  //    line above for as long as that tick is the last one, and gone the moment the
  //    scope is re-listed — which is the whole point of the repair and exactly why
  //    the repair cannot be the only trace. This note reads `relisted` off the
  //    header, which survives, and it is the one place a user can find out that a
  //    platform's backfill lost its record of what it had already done and started
  //    reading the list again.
  if (model.state?.relisted) {
    notes.push(t('popup.notes.relisted', {
      recorded: model.state.relisted.recorded,
      held: model.state.relisted.held,
      when: stampOf(model.state.relisted.at),
    }));
  }

  // 🔴 W30 · Why the summary line says "unknown", before the long-term coverage
  //    note: an unreadable count is current state, and the user asked for it.
  notes.push(...summaryNotes(model));

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
  // 🔴 W30: the summary line and its button. The button is always on screen, so
  //    it is always in the flattened text — and when it is disabled its reason
  //    is printed next to it, because a control nobody can use must come with
  //    the explanation rather than a tooltip a text report cannot show.
  lines.push(view.summary);
  lines.push(t('popup.buttonTag', { label: view.dashboard.label }));
  if (view.dashboard.reason) lines.push(view.dashboard.reason);
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

/**
 * 🔴 W18 · What the popup picks out of the snapshot is now the **header**, not the
 *    whole state: the debt ids moved to IndexedDB (lib/backfill/debt-store.ts) and
 *    `storage.local` holds the cursor, the counters and the halt record.
 *
 * The recognition test changed with it, in the strict direction: `pending` and
 * `archived` used to have to be *arrays*, now `pendingCount` and `archivedCount`
 * have to be *numbers*, and an array at either name is an explicit rejection. A
 * v1 record (a whole state, array ids and all) is therefore **not** accepted here
 * — it is the migration's input, and showing its stale numbers as live progress
 * would be exactly the "recording an unknown as known" mistake at the UI layer.
 */
function looksLikeState(value: unknown): value is BackfillHeader {
  if (!isHeader(value)) return false;
  // `isHeader` already refuses a record carrying id arrays; this adds the one
  // thing the popup needs on top of it, which is that the counts are usable
  // numbers rather than NaN/Infinity that would render as "owed NaN".
  return Number.isFinite(value.pendingCount) && Number.isFinite(value.archivedCount);
}

export function pickBackfillState(snapshot: Record<string, unknown> | null): BackfillHeader | null {
  if (!snapshot) return null;
  let best: BackfillHeader | null = null;
  for (const [key, value] of Object.entries(snapshot)) {
    if (!key.startsWith(STATE_KEY_PREFIX)) continue;
    if (!looksLikeState(value)) continue;
    // The key carries platform/scope and so does the value; only accept the two
    // agreeing, so a mismatched progress set can never be displayed.
    if (stateKey(value.platform, value.scope) !== key) continue;
    if (!best || value.archivedCount > best.archivedCount) best = value;
  }
  return best;
}

/** Every valid debt set in the snapshot (those whose key and value agree). Failure aggregation and clearing walk it. */
export function backfillStateEntries(
  snapshot: Record<string, unknown> | null,
): Array<{ key: string; state: BackfillHeader }> {
  if (!snapshot) return [];
  const out: Array<{ key: string; state: BackfillHeader }> = [];
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
