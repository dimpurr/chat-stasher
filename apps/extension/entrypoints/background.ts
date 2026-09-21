import {
  extractIdentity,
  extractSessionId,
  SCHEMA,
  pathSafeSessionId,
  findPlatformForUrl,
  getPlatformByOrigin,
  HOOK_STATUS_MESSAGE,
  isHookStatusMessage,
  PLATFORMS,
  type CapturedFetch,
  type InboxBundle,
} from '../lib/contract';
import { refreshBadge } from '../lib/badge';
import { browserLocalStore } from '../lib/backfill/store';
import {
  HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN,
  HOOK_DECLINE_UNREADABLE_MESSAGE,
  recordHookDecline,
  recordHookStatus,
} from '../lib/hook-status';
import { deliver, isItemRejected, isValidDeliverName } from '../lib/native-host';
import { contentFingerprint, isUnchangedSinceDelivery, rememberDelivered } from '../lib/recapture';
import {
  drainOutbox,
  enqueue,
  getEntry,
  outboxApiPresent,
  summary as outboxSummary,
  type EnqueueResult,
} from '../lib/outbox';
import { OUTBOX_ALARM_NAME, syncOutboxAlarm } from '../lib/outbox-alarm';
import {
  HOST_UNAVAILABLE,
  checkHost,
  loadHostPause,
  loadHostStatus,
  setHostPause,
  type HostStatusRecord,
} from '../lib/host-status';
import { HELLO_PROBE_TIMEOUT_MS } from '../lib/native-host';
import {
  BACKFILL_ENABLED_KEY,
  isBackfillEnabled,
  tickBackfill,
  tickBlockReason,
  type TickResult,
} from '../lib/backfill/schedule';
import { recordBackfillHalt, type BackfillOptions, type HttpPort } from '../lib/backfill/engine';
import type { LedgerRefusal } from '../lib/backfill/ledger';
import {
  armBackfillTick,
  BACKFILL_ALARM_NAME,
  BACKFILL_SAFETY_ALARM_NAME,
  findUnreadableState,
  isBackfillChainArmed,
  loadTargets,
  migrateLegacyScopes,
  rememberOrganizationScopedTarget,
  rememberTarget,
  saveLastTick,
  syncBackfillAlarm,
  type AlarmsApi,
  type BackfillTarget,
  type TabSweepTrace,
} from '../lib/backfill/alarm';
import { systemRandom, type RandomFn } from '../lib/backfill/random';
// 🔴 W31 · The scope a scoped plan's requests carry is read out of the page's own
//    captured URL, and the plan table is asked whether this platform is one of them.
import { backfillCapabilityOf, backfillPlanFor } from '../lib/backfill/enumerate';
import { isClaudeOrgId, orgFromRequestUrl, type OrgResolution } from '../lib/backfill/claude-org';
import { haltClassOf, haltStillApplies, isHeader, stateKey } from '../lib/backfill/types';
import {
  BACKFILL_PING_MESSAGE,
  askTabForClaudeOrg,
  isTabHello,
  pickLiveTab,
  rememberTab,
  sweepUnregisteredTabs,
  tabHttpPort,
  type TabSend,
  type TabSweepReport,
} from '../lib/backfill/tab-port';
import {
  POPUP_START_BACKFILL_MESSAGE,
  POPUP_STATUS_MESSAGE,
  type BackfillRuntimeStatus,
} from '../lib/popup-view';
import { initUiLocale } from '../lib/i18n';
/**
 * The outcome of one live-leg capture.
 *
 * 🔴 W2 · **The only value that counts as "it was stored" is `saved:true`, and
 *    it only appears when a matching ack arrived.** Queued-but-not-yet-confirmed
 *    is a separate, distinguishable outcome (`status:'queued'`) — neither a
 *    success nor a failure — and the badge counts it as "waiting".
 */
export interface HandledResult {
  /**
   * 🔴 true if and only if this payload received a matching `ack` (§1), or — with
   * status 'unchanged' — an identical copy (volatile fields aside, lib/recapture.ts)
   * already did, so it was not sent again.
   */
  saved: boolean;
  /** Five mutually exclusive outcomes, every one of which has to be sayable. */
  status: 'delivered' | 'unchanged' | 'queued' | 'rejected' | 'refused';
  reason?: string;
  /** The `nack` kind when it was rejected (§6.3). */
  kind?: string;
  /**
   * 🔴 C20: the identity that was **actually used to name** this on the way to
   * disk. The root cause was "the same identity expressed twice" — the debt key
   * came from the list API's items[].id, the file name came from "scrape it out
   * of the URL with a regex again", and nothing checked the two against each
   * other in between. Reporting this field faithfully is what lets the backfill
   * leg reconcile on the spot (see sinkVerdict in engine.ts).
   * undefined when saved:false (nothing was ever named).
   */
  sessionId?: string;
  /** This payload's name on this machine's host (§6.2's `name`, i.e. the shard's source_file). */
  finalName?: string;
  bytes?: number;
  /**
   * 🔴 There is only one channel now. The field is kept so that "there is no
   * degraded channel" is explicitly visible in the type: the `downloads` value,
   * and the whole automatic-download channel behind it, have been removed.
   */
  channel?: 'native-messaging';
}

/**
 * Build the inbox JSON document.
 * WHY raw-first: the parsed envelope is best-effort; if our field guesses are
 * wrong the CLI can re-derive structure from `raw.text` instead of losing data.
 */
function buildBundle(captured: CapturedFetch): InboxBundle {
  const parsed = { hasJson: false, keys: [] as string[] };
  try {
    const obj = JSON.parse(captured.text);
    if (obj && typeof obj === 'object') {
      parsed.hasJson = true;
      parsed.keys = Object.keys(obj);
    }
  } catch { /* not JSON */ }
  const sessionId = resolveSessionId(captured) ?? 'unknown';
  const platform = findPlatformForUrl(captured.url) ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  return {
    schema: SCHEMA,
    platform: platform?.id ?? 'deepseek',
    sessionId,
    // ADR-002: the dedupe axis is the ACCOUNT. `sessionId` guard keeps a
    // per-session id from ever being mistaken for the stable account id.
    identity: extractIdentity(captured.text, sessionId === 'unknown' ? null : sessionId),
    url: captured.url,
    method: captured.method,
    status: captured.status,
    capturedAt: new Date(captured.capturedAt).toISOString(),
    parsed,
    raw: {
      text: captured.text,
      bytes: new TextEncoder().encode(captured.text).length,
    },
  };
}

/**
 * 🔴 C21 · **The one place on this path that decides "whose conversation is this".**
 *
 * The order is not a preference, it is the root-cause fix itself:
 *  1. `captured.sessionId` — the authoritative value carried down from upstream
 *     when it **already knows** the identity (backfill leg: debt key = the list
 *     API's items[].id, see lib/backfill/engine.ts). When it is present we
 *     **never derive again** — the second expression dies right here.
 *  2. Only without it do we fall back to extractSessionId (live leg: the identity
 *     exists in the URL and nowhere else).
 *
 * A payload from a page can never take branch 1: lib/contract.ts's
 * isCapturedFetchShape **rejects on sight** any page payload carrying a sessionId.
 */
function resolveSessionId(captured: CapturedFetch): string | null {
  if (captured.sessionId !== undefined) return captured.sessionId;
  return extractSessionId(captured.url, captured.text, captured.pageUrl);
}

export async function handleCaptured(captured: CapturedFetch): Promise<HandledResult> {
  const prepared = preparePayload(captured);
  if (!prepared.ok) {
    return { saved: false, status: 'refused', reason: prepared.reason };
  }
  const { name, payload, bytes, sessionId } = prepared;

  // Unchanged since its last acknowledged delivery ⇒ do not append another
  // identical copy (lib/recapture.ts). Only platforms with known volatile fields
  // get a fingerprint; everything else is always delivered.
  const recaptureStore = browserLocalStore();
  const platformId = findPlatformForUrl(captured.url)?.id ?? null;
  const fingerprint = platformId ? await contentFingerprint(platformId, captured.text) : null;
  if (fingerprint && await isUnchangedSinceDelivery(recaptureStore, name, fingerprint)) {
    return {
      saved: true,
      status: 'unchanged',
      finalName: name,
      bytes,
      sessionId,
      channel: 'native-messaging',
    };
  }

  // 🔴 ADR-025 §10 · **write-ahead**: get it into the outbox first, and only
  //    then attempt any delivery. If the SW is killed between these two lines,
  //    the conversation is still waiting on disk and the next drain sends it.
  const queued: EnqueueResult = await enqueue(name, payload);
  if (!queued.accepted || !queued.sha256) {
    // Cannot get it into the outbox ⇒ do not deliver. Something delivered that
    // the outbox does not know about is precisely what write-ahead exists to
    // prevent; better to report an honest refusal than to quietly drop a layer
    // of protection.
    return {
      saved: false,
      status: 'refused',
      reason: queued.reason ?? 'outbox-unavailable',
      finalName: name,
      bytes,
      sessionId,
    };
  }

  // Try one drain right after enqueueing (the second of task 5's two occasions;
  // the first is the alarm heartbeat).
  const drained = await drainSafely();
  await syncOutboxAlarmSafely();

  const lookup = await getEntry(queued.sha256);
  await refreshBadgeSafely();

  if (!lookup.ok) {
    // 🔴 "Could not read it" is neither "delivered" nor "not delivered". This
    //    item really is in the outbox, but we cannot say what state it is in —
    //    so say we cannot, and never guess it into saved.
    return {
      saved: false,
      status: 'queued',
      reason: 'outbox-unavailable',
      finalName: name,
      bytes,
      sessionId,
    };
  }
  if (lookup.entry === null) {
    // 🔴 Only a matching ack deletes an entry (§1), so "not found" = this one really was stored.
    // 🔴 And only now may the fingerprint be written down (lib/recapture.ts): a copy
    //    that was merely queued proves nothing, and skipping it next time on that
    //    basis would lose a conversation that was never stored. Failing to record
    //    it is not a delivery failure — the cost of a missing record is one extra
    //    copy, so the ack's outcome must not be touched by it.
    if (fingerprint) {
      try {
        await rememberDelivered(recaptureStore, name, fingerprint);
      } catch (err) {
        // Metadata only (platform + fingerprint prefix): never a URL, id or body.
        console.warn(
          '[chat-stasher] could not record the delivered fingerprint for'
          + ` ${platformId}/${fingerprint.slice(0, 12)}`,
          (err as Error).message,
        );
      }
    }
    return {
      saved: true,
      status: 'delivered',
      finalName: name,
      bytes,
      sessionId,
      channel: 'native-messaging',
    };
  }
  const entry = lookup.entry;
  if (entry.state === 'rejected') {
    return {
      saved: false,
      status: 'rejected',
      reason: entry.lastError ?? 'host rejected this delivery',
      kind: entry.rejectKind,
      finalName: name,
      bytes,
      sessionId,
    };
  }
  return {
    saved: false,
    status: 'queued',
    reason: entry.lastError ?? (drained.stoppedBy === 'outbox-unavailable'
      ? 'outbox-unavailable'
      : 'not acknowledged yet (queued in the outbox)'),
    finalName: name,
    bytes,
    sessionId,
  };
}

/** The single construction point for names and payloads. The live leg and the backfill leg **share** it, so the on-disk logic does not fork. */
export type PreparedPayload =
  | { ok: true; name: string; payload: string; bytes: number; sessionId: string }
  | { ok: false; reason: string };

function preparePayload(captured: CapturedFetch): PreparedPayload {
  const sessionId = resolveSessionId(captured);
  if (cancelledIdLike(sessionId)) {
    // Per-session naming is the inbox contract; a session-less capture has no
    // stable file name and is dropped rather than polluting the inbox.
    return { ok: false, reason: 'no-session-id (skipped in report only)' };
  }

  const bundle = buildBundle(captured);
  // 🔴 C21 · Naming is an **identity mapping**, not "replace unsafe characters":
  //    sanitizePathSegment is many-to-one ('a b' and 'a/b' collide), and the old
  //    download path overwrote ⇒ two different conversations could erase each
  //    other. If a safe name cannot be produced, stop on the spot and leave a
  //    trace; never force one through.
  const named = pathSafeSessionId(bundle.sessionId);
  if (named === null) {
    return { ok: false, reason: 'session id is not usable as a file name (refused, not collapsed)' };
  }
  const name = `${bundle.platform}-${named}.json`;
  // §6.2's name rule. Our builder only ever produces legal names anyway; this is
  // the defensive final gate: refuse on the spot rather than send a name the
  // host would certainly nack.
  if (!isValidDeliverName(name)) {
    return { ok: false, reason: `refused to build a deliver name outside §6.2: ${named.length} chars` };
  }
  const payload = JSON.stringify(bundle);
  return {
    ok: true,
    name,
    payload,
    bytes: new TextEncoder().encode(payload).byteLength,
    sessionId: bundle.sessionId,
  };
}

/** Drain: it must never be allowed to take the "user's current conversation" path down with it. */
async function drainSafely(): Promise<Awaited<ReturnType<typeof drainOutbox>>> {
  try {
    return await drainOutbox();
  } catch (err) {
    console.warn('[chat-stasher] outbox drain failed', (err as Error).message);
    return { attempted: 0, delivered: 0, rejected: 0, waiting: 0, stoppedBy: 'outbox-unavailable' };
  }
}

/**
 * §10: keep the outbox's own retry timer while it holds pending items, and clear
 * it once it is empty. Independent of the backfill switch. Never throws into a
 * delivery path.
 */
async function syncOutboxAlarmSafely(): Promise<void> {
  try {
    // No IndexedDB API ⇒ the outbox cannot exist ⇒ 0 is certain, not assumed.
    // A failed read with the API present stays unknown (null) and keeps the timer.
    if (!outboxApiPresent()) {
      await syncOutboxAlarm(alarmsApi(), 0);
      return;
    }
    const s = await outboxSummary();
    await syncOutboxAlarm(alarmsApi(), s === null ? null : s.pending);
  } catch (err) {
    console.warn('[chat-stasher] outbox alarm sync failed', (err as Error).message);
  }
}

/**
 * The badge is decorative: an error in it must not affect any real path.
 * 🔴 But it **has to be awaited**: without that, the caller only gets "the badge
 *    is about to be repainted", while the badge says exactly "how many items the
 *    outbox is holding" — and that number must be in step with this result.
 */
async function refreshBadgeSafely(): Promise<void> {
  try {
    await refreshBadge();
  } catch (err) {
    console.warn('[chat-stasher] badge update failed', (err as Error).message);
  }
}

/**
 * 🔴 W2 · **The backfill leg's exit. It does not go through the outbox.** (§10)
 *
 * Reason: this conversation still exists on the platform, and the debt set is
 * where it belongs — queueing a payload would make the same conversation exist
 * once in the "debt" ledger and once in the "outbox" ledger.
 *
 * Three outcomes, none of which may be confused with another:
 *  · a matching ack ⇒ saved:true, and the engine clears the debt;
 *  · item-scope, non-retryable nack (§6.3, `isItemRejected`) ⇒ saved:false (judged dead):
 *    the engine records a failure and moves on to the next item;
 *  · everything else (timeout, host missing, host-scope nack such as `config`) ⇒ retryLater:
 *    the debt stays untouched and this leg pauses until a `hello` succeeds.
 */
export async function deliverBackfillItem(captured: CapturedFetch): Promise<{
  saved: boolean;
  reason?: string;
  sessionId?: string;
  retryLater?: boolean;
}> {
  const prepared = preparePayload(captured);
  if (!prepared.ok) return { saved: false, reason: prepared.reason };

  const result = await deliver(prepared.name, prepared.payload);
  if (result.delivered) {
    return { saved: true, sessionId: prepared.sessionId };
  }
  if (isItemRejected(result)) {
    return {
      saved: false,
      reason: `host rejected the delivery (${result.kind ?? result.reason})`,
      sessionId: prepared.sessionId,
    };
  }
  // 🔴 The exit is unreachable: keep the debt, pause with a trace (§10, "visible reason").
  await setHostPause(browserLocalStore(), {
    reason: HOST_UNAVAILABLE,
    at: Date.now(),
    detail: result.detail ? `${result.reason}: ${result.detail}` : result.reason,
  });
  return { saved: false, reason: HOST_UNAVAILABLE, retryLater: true };
}

/**
 * The one question the popup asks when it opens: "is the host there, and where
 * does it write?"
 * The probe window is short (HELLO_PROBE_TIMEOUT_MS): the popup is UI and must
 * not be hung up on a stuck host; the delivery path still uses the 60 seconds
 * §2 specifies. The conclusion is written to storage.local — writing it down is
 * the only way it is still visible the next time the popup opens.
 */
export async function hostStatusForPopup(): Promise<HostStatusRecord | null> {
  try {
    return await checkHost(browserLocalStore(), { timeoutMs: HELLO_PROBE_TIMEOUT_MS });
  } catch (err) {
    console.warn('[chat-stasher] host check failed', (err as Error).message);
    return await loadHostStatus(browserLocalStore());
  }
}

// ---------------------------------------------------------------------------
// C13 → C19 · Wiring the backfill leg
//
// C13 triggered it from the **live leg's onMessage**, because under MV3 the SW is
// normally dead and that message already carries platform/origin/account scope.
// Those reasons still hold, so the kick **stays**. But it has one fatal
// corollary:
// 🔴 a user who installs the extension and never opens that site again will never
//    get a second live capture to kick it, so their history would never finish
//    backfilling.
// ⇒ C19 added a second heartbeat: chrome.alarms (see lib/backfill/alarm.ts; the
//   period and its argument are there). Both heartbeats go through the same
//   tickBackfill, with identical gate ordering.
//   When the alarm wakes, the SW is brand new and knows nothing — so the live
//   leg's kick also records the target into storage (rememberTarget), and the
//   alarm just reads it, without guessing a single thing.
// ---------------------------------------------------------------------------

/**
 * 🔴 The network port used for explicit overrides. null by default.
 * Through C13–C18 this was the **only** injection point, and no production code
 * called it ⇒ the backfill leg always stopped at 'no-http-port'. Since C19 it has
 * degenerated into a test seam: the real production port is built on the spot by
 * resolveHttpPort() from "a live, logged-in platform tab".
 */
let backfillTransport: HttpPort | null = null;
let lastTick: TickResult | null = null;
let pendingTick: Promise<unknown> = Promise.resolve();
/**
 * Test seam: inject a fake clock / a custom pacing / a deterministic source of
 * randomness so tests do not really sleep 20 seconds and do not have to sample
 * a jittered interval. Always null in production.
 *
 * 🔴 W16 · `random` joined this seam because the jitter is now the *default*:
 *    a test that pins an exact wait (or an exact total clock advance) has to be
 *    able to say what the draw is, and `() => 0` is the boundary that reproduces
 *    the old deterministic numbers exactly.
 */
interface BackfillSeam {
  pace?: BackfillOptions['pace'];
  clock?: BackfillOptions['clock'];
  random?: RandomFn;
}
let backfillPaceOverride: BackfillSeam | null = null;

/** The draw source the *runtime* uses for its own decisions (arming the alarm). Production unless a test says otherwise. */
function backfillRandom(): RandomFn {
  return backfillPaceOverride?.random ?? systemRandom;
}

export function configureBackfillTransport(http: HttpPort | null): void {
  backfillTransport = http;
}

export function configureBackfillPace(override: BackfillSeam | null): void {
  backfillPaceOverride = override;
}

/** The most recent tick's result (for tests and diagnosis, and for C18's popup). */
export function lastBackfillTick(): TickResult | null {
  return lastTick;
}

function alarmsApi(): AlarmsApi | null {
  return (browser as unknown as { alarms?: AlarmsApi }).alarms ?? null;
}

function tabsApi(): {
  sendMessage: TabSend;
  /**
   * 🔴 W51 · `chrome.tabs.query({})` returns `id` / `discarded` / `frozen` /
   *    `status` **without** the `tabs` permission; only `url` / `title` are
   *    redacted. Missing ⇒ the sweep cannot look, which is `{ looked: false }`,
   *    not "there are no tabs".
   */
  query: (() => Promise<Array<{ id?: number; discarded?: boolean; frozen?: boolean }>>) | null;
} | null {
  const tabs = (browser as unknown as {
    tabs?: {
      sendMessage?: TabSend;
      query?: (info: Record<string, never>) => Promise<Array<{
        id?: number;
        discarded?: boolean;
        frozen?: boolean;
      }>>;
    };
  }).tabs;
  // 🔴 tabs.sendMessage does not need the 'tabs' permission ('tabs' only governs
  //    sensitive fields like url/title), and we only message **our own injected
  //    content script**. The matches need not change a character. `tabs.query`
  //    of `{}` is the same permission story: we never read `url` / `title`.
  if (!tabs || typeof tabs.sendMessage !== 'function') return null;
  return {
    sendMessage: (id, msg) => tabs.sendMessage!(id, msg),
    query: typeof tabs.query === 'function' ? () => tabs.query!({}) : null,
  };
}

/**
 * 🔴 W51 · One unattended recovery pass, used only from the alarm tick.
 *
 * Not called from `resolveHttpPort`: that helper is also the popup's
 * `transportWired` probe, and a sweep there would fan out `BACKFILL_PING_MESSAGE`
 * to every open tab on every failed resolution. The alarm tick is the one
 * place a registered target is about to concede `no-http-port` without a
 * human looking at the page.
 */
async function recoverUnregisteredTabs(): Promise<TabSweepReport> {
  const tabs = tabsApi();
  if (!tabs || tabs.query === null) return { looked: false };
  return sweepUnregisteredTabs(
    browserLocalStore(),
    tabs.query,
    (id) => tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }),
  );
}

function persistSweep(report: TabSweepReport | null): TabSweepTrace | null {
  if (report === null) return null;
  if (!report.looked) return { looked: false };
  return {
    looked: true,
    queried: report.queried,
    pruned: report.pruned,
    pinged: report.pinged,
    registered: report.registered,
    deferred: report.deferred,
    crowded: report.crowded,
  };
}

/**
 * 🔴 **This is where the http port is built in production.**
 *
 * Order: explicit override (tests) → the specified tab → any live same-origin tab
 * in the registry. None at all ⇒ undefined ⇒ tickBackfill answers 'no-http-port'
 * faithfully.
 * 🔴 It never degrades into "the SW fetches it itself" — that would need host
 *    permissions and would move fetching out of the user's logged-in context.
 *    Better not to run this time.
 */
async function resolveHttpPort(origin: string, senderTabId?: number): Promise<HttpPort | undefined> {
  if (backfillTransport) return backfillTransport;
  const tabs = tabsApi();
  if (!tabs) return undefined;
  // On the live leg's path, the tab that sent the message **is open right now and
  // has just performed a real capture** — it is the most reliable fetch channel,
  // and there is no need to consult the registry again.
  if (senderTabId !== undefined) return tabHttpPort(senderTabId, tabs.sendMessage);
  const live = await pickLiveTab(browserLocalStore(), origin, (id) =>
    tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
  return live ? tabHttpPort(live.tabId, tabs.sendMessage) : undefined;
}

/**
 * C18 · The runtime facts the popup asks background for. All of them are "what is
 * actually true on my side", with no inference.
 * 🔴 transportWired is not a static flag but the result of **a ping taken right
 *    now**: is there a live platform tab that can fetch on our behalf. If there
 *    is not, there is not, and the popup says so plainly.
 */
export async function backfillRuntimeStatus(): Promise<BackfillRuntimeStatus> {
  // 🔴 C33: one ping answers both questions at once (is the channel up / which
  //    platform is it). Asking twice would allow a self-contradictory answer:
  //    "there is a channel, but I cannot say which platform".
  const live = await liveTransport();
  return {
    transportWired: live.wired,
    lastTickReason: lastTick?.reason ?? null,
    liveTarget: live.target,
  };
}

/**
 * 🔴 W2 · The "is the host there" question the popup asks.
 *
 * Background asks it rather than the popup probing for itself, for two reasons:
 * the answer lands in storage.local as soon as it is written, so closing and
 * reopening the popup still shows the last conclusion; and this is the only place
 * that touches the host at all — the popup has not one line of network/socket code.
 */
export async function popupHostStatus(): Promise<BackfillRuntimeStatus> {
  const base = await backfillRuntimeStatus();
  return { ...base, nativeHost: await hostStatusForPopup() };
}

/**
 * The fetch channel that is live right now.
 * `wired` is byte-for-byte synonymous with C19's hasLiveTransport; `target` is a
 * fact it brings along: that tab's origin, and the platform that origin maps to
 * in the platform table.
 * 🔴 Under an explicit override (the test seam) target is always null — there is
 *    no tab on that path, and naming a platform would be a lie.
 */
async function liveTransport(): Promise<{
  wired: boolean;
  target: { platform: string; origin: string } | null;
  /**
   * 🔴 W31c · **The tab the channel is live on.** The registration below has to
   * ask *this* page which organization it is using, and it has just proved it is
   * alive — asking a second time (as `pickLiveTab` would) could reach a different
   * tab, or none. `null` under the explicit override, where there is no tab at all.
   */
  tabId: number | null;
}> {
  if (backfillTransport) return { wired: true, target: null, tabId: null };
  const tabs = tabsApi();
  if (!tabs) return { wired: false, target: null, tabId: null };
  const live = await pickLiveTab(browserLocalStore(), null, (id) =>
    tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
  if (!live) return { wired: false, target: null, tabId: null };
  const row = getPlatformByOrigin(live.origin);
  // The origin is not in the platform table ⇒ we cannot answer "which platform is
  // this" ⇒ say null plainly, never guess one.
  return { wired: true, target: row ? { platform: row.id, origin: live.origin } : null, tabId: live.tabId };
}

/**
 * 🔴 W31c · **The scope a scoped platform's target carries until its page has
 * been asked, and the word this repository already uses for "the account
 * identifier cannot be told".**
 *
 * It is a *registration* value and never a request value: lib/backfill/engine.ts
 * refuses it for a plan whose paths carry the scope, before any request, precisely
 * so that it can sit in the registry as "not known yet" without ever being
 * substituted into a URL.
 */
export const UNRESOLVED_SCOPE = 'default';

/**
 * 🔴 W49 · **Store one organization per scoped-platform row, and drop every
 * row for that platform whose scope is not an organization.**
 *
 * `rememberTarget` dedups by platform+scope, so a leftover conversation title
 * (or the `'default'` sentinel) is a different key from the organization that
 * just became known and is left in the registry. `forgetTarget` is the same
 * key: calling it with the current row's scope cannot see a different one.
 * The alarm then wakes for a scope that names no account.
 *
 * A value that is not an organization id is stored as `UNRESOLVED_SCOPE` — the
 * existing spelling for "the identifier could not be told" — rather than
 * written as if it were one, and only when this platform has no organization
 * row: collapsing a title must not put `'default'` in front of a live
 * organization (the alarm would then halt on the sentinel and never tick the
 * org). Another organization for the same platform is kept: two organizations
 * are two targets. One registry write; dropped non-organization rows lose
 * their local ledger header (the host archive is not touched).
 */
async function rememberScopedTarget(
  store: ReturnType<typeof browserLocalStore>,
  target: BackfillTarget,
): Promise<BackfillTarget[]> {
  if (!backfillPlanFor(target.platform)?.scopeInPath) {
    return await rememberTarget(store, target);
  }
  const scope = isClaudeOrgId(target.scope) ? target.scope : UNRESOLVED_SCOPE;
  return await rememberOrganizationScopedTarget(
    store, { ...target, scope }, isClaudeOrgId,
  );
}

/**
 * 🔴 W31c · **Who can turn "this page" into an account scope.**
 *
 * The question this answers is not "which platform is this" (the platform table
 * does that) but "this platform needs an account identifier that is not in the
 * page URL — can we get it?". Only one platform can, and only from its own page:
 * claude.ai's organization lives in a cookie and behind a same-origin endpoint,
 * which is why the ask travels over the tab channel (lib/backfill/tab-port.ts).
 *
 * 🔴 It is a **lookup, not a wildcard**: a platform that declares `scopeInPath`
 *    and has no entry here keeps the pre-W31c behaviour (the target is registered
 *    with the unresolved sentinel and the engine halts by name), rather than
 *    having some other platform's resolver run against its page. A second scoped
 *    platform gets its own entry, and this table is where that decision is made.
 */
export type ScopeResolver = (tabId: number | null, origin: string) => Promise<OrgResolution>;

export function scopeResolverFor(platform: string): ScopeResolver | null {
  return platform === 'claude' ? claudeScopeFromTab : null;
}

/**
 * 🔴 W31c · **Ask a live claude.ai page which organization it is using.**
 *
 * `tabId` is the tab the caller already proved alive (registration); `null` means
 * "find one" (the alarm's path, which holds no tab). Either way the question is
 * put to the page and the answer is passed back unchanged — this function decides
 * nothing about organizations.
 *
 * 🔴 Every way of failing here is the resolver's transient `transport-error`
 *    (`askTabForClaudeOrg`), and it is deliberately not "no organization": a
 *    closed tab, a reloaded extension and a wedged renderer are facts about the
 *    channel, and the account is not implicated by any of them.
 */
async function claudeScopeFromTab(tabId: number | null, origin: string): Promise<OrgResolution> {
  const tabs = tabsApi();
  if (!tabs) {
    return { ok: false, halt: 'transport-error', detail: 'the tabs API is not available, so the page cannot be asked' };
  }
  if (tabId === null) {
    const live = await pickLiveTab(browserLocalStore(), origin, (id) =>
      tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
    if (!live) {
      return {
        ok: false,
        halt: 'transport-error',
        detail: 'no open page of that platform answered, so its organization could not be asked for',
      };
    }
    return askTabForClaudeOrg(live.tabId, tabs.sendMessage);
  }
  return askTabForClaudeOrg(tabId, tabs.sendMessage);
}

/**
 * 🔴 W31c · **Is it worth asking the page again yet?**
 *
 * The alarm re-asks a target whose scope is unresolved, because that is the only
 * thing that retries a *transient* resolution failure (see the registration below:
 * the popup's start button is hidden once a target exists, so nothing else will).
 * Two records say "do not spend a question on this yet":
 *
 *  · **a permanent halt** for that scope. Its reason is already the answer the
 *    resolver would reach again — several organizations and no signal, or none at
 *    all — and re-asking every tick would turn "stop and wait for a human" into a
 *    slow poll of the account. The remedy the popup names is a human action
 *    (opening a conversation), and that arrives as a real capture, which
 *    registers the organization's own target and clears the path by itself;
 *  · **a transient halt whose backoff has not elapsed**. This is the engine's own
 *    rule (engine.ts: a waiting round issues no request and writes nothing), and
 *    re-asking before `retryAt` would both spend the question early and push the
 *    ladder's next rung further out.
 *
 * 🔴 W44 · **A capability-class record is asked a third question, and it is not
 *    "permanent".** "Permanent" in the engine's sense means "waiting does not
 *    change it" — true of `unsupported-platform`, and beside the point: what
 *    changed it was **this build**, and the engine's own expiry check is what
 *    notices. Holding the question back on the strength of such a record would
 *    reproduce the W44 defect one layer up: the plan arrives, the engine would run,
 *    and the question that gives the run a scope is never asked because a record
 *    about a *capability* says the leg is stopped. So the record is asked the same
 *    question the engine asks (`haltStillApplies`), and a record that no longer
 *    applies does not stand in the way.
 *
 * 🔴 `now` is a parameter rather than a call to `Date.now()` here: this is a
 *    decision about a clock, and a decision about a clock that cannot be handed a
 *    clock cannot be tested at its boundary.
 */
export async function scopeRetryDue(
  store: ReturnType<typeof browserLocalStore>,
  platform: string,
  scope: string,
  now: number,
): Promise<boolean> {
  if (!store) return false;
  const raw = await store.load(stateKey(platform, scope));
  // No record, or one this build cannot read: nothing has been decided, so ask.
  // (An unreadable record already halted the engine by name; asking again is not
  // the thing that would make it worse, and refusing to ask would freeze a target
  // for a reason that is about parsing, not about organizations.)
  if (!isHeader(raw) || raw.halted === null) return true;
  // 🔴 R44 · A halt that no longer applies is not a reason to stay silent: the
  //    whole point of W44 is that such a record stops deciding anything, so the
  //    question it once answered has to be asked again. Returning false here made
  //    this layer a no-op — capability reasons are permanent, so the next line
  //    returned false anyway, and the target stayed frozen exactly as before.
  if (!haltStillApplies(raw.halted, backfillCapabilityOf(platform))) return true;
  if (haltClassOf(raw.halted.reason) === 'permanent') return false;
  return raw.halted.retryAt === undefined || now >= raw.halted.retryAt;
}

/**
 * 🔴 C33 · The registration entry point for **explicit informed consent**: the
 * user pressed "start backfilling this platform" in the popup.
 *
 * Its relationship to kickBackfill: **this is one more registration entry point,
 * not a relaxation of any existing decision.** Nothing in kickBackfill /
 * rememberTab / the alarm's `targets.length` test moved a character — what
 * happens here is exactly the rememberTarget line inside kickBackfill, with the
 * target's source changed from "a real capture" to "the user pressing this".
 * Neither source is invented by us.
 *
 * 🔴 What about scope (the account identifier), for a platform whose requests
 * carry one: **this is where W31c asks the page.**
 *  · Before W31c this function recorded `'default'` for every platform, and the
 *    comment here said the channel "brings back the origin only, no account
 *    information at all". For a scoped platform that was fatal: 'default' is not
 *    an organization, engine.ts refuses it by name before any request, and the
 *    start button therefore halted `org-unresolved` forever. The page **is**
 *    asked now, through `scopeResolverFor` — one question over the same tab
 *    channel, whose answer is the resolver's decision (an organization, or a
 *    named halt), not a guess (lib/backfill/claude-org.ts).
 *  · For a platform with no resolver the old sentence still holds exactly: the
 *    channel brings back the origin only, and 'default' is recorded. That word is
 *    not an invention of this function — it is this repository's existing spelling
 *    for "the account cannot be told" (`backfillTargetFor`'s
 *    `identity.value || 'default'`, lib/popup-view.ts's emptyStateFor,
 *    lib/contract.ts's IdentityLevel 'default').
 *  · A **failed** resolution still registers the target, carrying
 *    `UNRESOLVED_SCOPE`, and writes the named halt down
 *    (`recordBackfillHalt`) so the popup can say which of the four facts it is.
 *    Registering rather than staying silent is what gives the alarm something to
 *    retry a transient failure through: once a target exists this popup's button
 *    is hidden (`canStartBackfillHere`), so nothing else would ever ask again.
 *    The engine then refuses to tick it by name, before any request, so a row
 *    with no organization still fetches nothing.
 *  · The cost (written down honestly, and unchanged from C33): a scoped target
 *    that first registered as unresolved and is later resolved is replaced by one
 *    carrying the real organization (`rememberScopedTarget` in the alarm's path).
 *    The unresolved row is dropped, and that scope's local ledger header
 *    (`cs_backfill_v2:<platform>:<scope>` — halt, cursor, failures) is removed
 *    with it: a run already opened that header before any request, and the
 *    popup walks every header, so leaving it would show an abandoned halt
 *    next to the organization's empty ledger. Nothing is sent to the host
 *    archive under that string (the archive is append-only and is not
 *    touched). A later real capture registers the organization's own target
 *    anyway. The same replacement drops a leftover conversation title: that
 *    string is not an organization, and leaving it would have the alarm wake
 *    for a scope that names no account.
 */
export async function registerBackfillTargetHere(): Promise<
  | { ok: true; target: { platform: string; origin: string; scope: string } }
  | {
    ok: false;
    reason: 'no-store' | 'no-live-transport' | 'origin-not-a-platform'
      | 'org-ambiguous' | 'org-unresolved' | 'transport-error';
  }
> {
  const store = browserLocalStore();
  if (!store) return { ok: false, reason: 'no-store' };
  const live = await liveTransport();
  if (!live.wired) return { ok: false, reason: 'no-live-transport' };
  if (!live.target) return { ok: false, reason: 'origin-not-a-platform' };
  const { platform, origin } = live.target;
  let scope = UNRESOLVED_SCOPE;
  const resolver = scopeResolverFor(platform);
  if (resolver) {
    const resolved = await resolver(live.tabId, origin);
    if (resolved.ok) {
      scope = resolved.org;
    } else {
      await recordBackfillHalt(store, {
        platform, scope, reason: resolved.halt, detail: resolved.detail,
      });
      await rememberScopedTarget(store, { platform, origin, scope, at: Date.now() });
      return { ok: false, reason: resolved.halt };
    }
  }
  const target = { platform, origin, scope };
  await rememberScopedTarget(store, { ...target, at: Date.now() });
  return { ok: true, target };
}

/** Wait for the fire-and-forget tick to finish. The backfill leg must never slow the write-down path, so this is the only way to wait. */
export function backfillTickSettled(): Promise<unknown> {
  return pendingTick;
}

/** Derive a backfill target from one real capture. Returns null when it cannot be derived (no guessing). */
export function backfillTargetFor(
  captured: CapturedFetch,
): { platform: string; origin: string; scope: string } | null {
  const row = findPlatformForUrl(captured.url)
    ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  if (!row) return null;
  let origin: string | null = null;
  for (const candidate of [captured.pageUrl, captured.url]) {
    if (!candidate) continue;
    try {
      const o = new URL(candidate).origin;
      if (row.origins.includes(o)) { origin = o; break; }
    } catch { /* not a valid URL ⇒ try the next candidate */ }
  }
  if (!origin) return null;
  /**
   * 🔴 W31 · **A plan whose paths carry the scope takes it from the page's own
   * request, and only from there.**
   *
   * claude.ai addresses every conversation by organization, and the value is not in
   * the page URL — but it *is* in the URL of the request that produced this
   * capture, because every request the page makes carries it. So for such a plan the
   * scope is read out of that URL (`orgFromRequestUrl`) and **the identity
   * heuristic is not consulted at all**: this is the "the page's own requests win"
   * source of lib/backfill/claude-org.ts, and it is evidence about this page rather
   * than a guess about an account.
   *
   * 🔴 Returning **null** when the URL carries no organization is the point: no
   *    target is registered, so no tick runs, so no list request is sent and no
   *    `'default'` sentinel ever reaches a path. Registering the target with
   *    'default' would be worse than doing nothing — see the engine's refusal of
   *    that word for a scoped plan.
   */
  if (backfillPlanFor(row.id)?.scopeInPath) {
    const scope = orgFromRequestUrl(captured.url);
    // 🔴 W49 · orgFromRequestUrl already refuses a segment that is not an
    //    organization id (a conversation title, the unresolved sentinel). A
    //    null here is "no target", not a title written as if it were one.
    return scope === null ? null : { platform: row.id, origin, scope };
  }
  // The archive-scope key follows ADR-002's account axis; an untellable account
  // is 'default' (consistent with the write-down path).
  const identity = extractIdentity(captured.text, extractSessionId(captured.url, captured.text, captured.pageUrl));
  return { platform: row.id, origin, scope: identity.value || 'default' };
}

/**
 * After the live leg stores one item it kicks the backfill leg. Best-effort
 * throughout: any exception only reaches the log and never affects the write-down
 * of the user's current conversation.
 */
export async function kickBackfill(
  captured: CapturedFetch,
  senderTabId?: number,
): Promise<TickResult | null> {
  const target = backfillTargetFor(captured);
  if (!target) return null;
  const store = browserLocalStore();
  // Useful when the alarm wakes: record this target the user really did use, so
  // there is nothing to guess later. A failed write still lets this tick run —
  // the registry only affects the alarm's path.
  try {
    await rememberScopedTarget(store, { ...target, at: Date.now() });
  } catch (err) {
    console.warn('[chat-stasher] backfill target registry write failed', (err as Error).message);
  }
  const result = await tickBackfill({
    ...target,
    store,
    http: await resolveHttpPort(target.origin, senderTabId),
    // The archive exit = the backfill leg's own delivery function (**not through
    // the outbox**; see §10 and deliverBackfillItem).
    // 🔴 C20: **the return is mandatory**. This used to be
    //    `{ await handleCaptured(c); }` — the braces swallowed the HandledResult,
    //    so the engine heard no objection and cleared the debt even when nothing
    //    had been stored.
    sink: (c) => deliverBackfillItem(c),
    ...(backfillPaceOverride ?? {}),
  });
  lastTick = result;
  return result;
}

/**
 * 🔴 The alarm's kick. It goes through the **same tickBackfill** as kickBackfill,
 * with identical gate ordering.
 * Its only difference from the live leg: the target is read from the registry in
 * storage rather than taken from a message that just arrived.
 * One alarm clears at most 1 debt (DEFAULT_TICK_DETAILS); once it runs, it stops.
 *
 * 🔴 W16 · **This function is also where the next tick is armed**, at the very
 *    end, with a fresh draw from `[5, 10]` minutes. The alarm that woke us was a
 *    one-shot and the browser has already removed it, so the line below is the
 *    only thing standing between "the leg is running" and "the leg has silently
 *    stopped" — which is why it runs on *every* exit path, including the ones
 *    that did no work at all, and why it is in a `finally`-style position after
 *    the tick rather than inside the tick's own logic.
 */
export async function runAlarmTick(): Promise<TickResult> {
  try {
    return await runAlarmTickBody();
  } finally {
    await rearmBackfillTick();
  }
}

/**
 * Arm the next jittered tick. Best-effort: the safety alarm below is the
 * guarantee, this is the fast path, and a failure here must never turn a tick
 * that did its work into a thrown error.
 */
async function rearmBackfillTick(): Promise<void> {
  const alarms = alarmsApi();
  if (!alarms) return;
  try {
    await armBackfillTick(alarms, backfillRandom());
  } catch (err) {
    console.warn('[chat-stasher] backfill alarm re-arm failed', (err as Error).message);
  }
}

/**
 * 🔴 W31c · **The scope one alarm tick should run under, asking the page when the
 * registered one is not an organization.**
 *
 * The case this exists for is the target a failed registration left behind (see
 * `registerBackfillTargetHere`): it carries `UNRESOLVED_SCOPE`, so the engine would
 * refuse it by name forever. Asking again here is the **only** retry a transient
 * resolution failure gets — once any target exists, the popup's start button is
 * hidden — and it is also how a permanent one clears without a new capture: the
 * resolver's first source is the page's own requests, so the moment the page shows
 * an organization the next tick picks it up.
 *
 * 🔴 `scopeRetryDue` is asked **before** the question, so a tick that is inside a
 *    backoff (or that is standing on a permanent halt) spends nothing and simply
 *    falls through to the engine, which reports the halt that is already written.
 *
 * 🔴 When the question succeeds, the sentinel row is **replaced**, not joined: a
 *    target whose scope names no account is not a target, and leaving it in the
 *    registry would have the alarm wake for it every tick (`rememberScopedTarget`).
 *    The same replacement applies to a leftover conversation title: that string
 *    is not an organization. Collapsing it must not insert `'default'` in front
 *    of a live organization — the alarm `break`s on the sentinel halt and the
 *    org would starve. If an organization row already exists, this tick runs
 *    under that organization. The dropped row's local ledger header is removed
 *    with it; the host archive is not touched.
 */
async function resolveScopeForTick(
  store: ReturnType<typeof browserLocalStore>,
  target: { platform: string; origin: string; scope: string },
): Promise<string> {
  if (!backfillPlanFor(target.platform)?.scopeInPath) return target.scope;
  // 🔴 W49 · Only an organization id is "already resolved". `'default'` is the
  //    sentinel, and a conversation title is the same kind of fact: it names no
  //    account. Treating either as an organization would substitute it into
  //    `/api/organizations/<scope>/…`.
  if (isClaudeOrgId(target.scope)) return target.scope;
  // 🔴 W49b · Collapse through the one-write path. If a live organization is
  //    already registered, that path drops the title (and its ledger) *without*
  //    inserting `'default'`, and this tick runs under the organization rather
  //    than halting on the sentinel and `break`ing. A stale snapshot of a title
  //    the live leg already replaced is the same fact: re-inserting `'default'`
  //    in front of the org the capture just registered is the race this stops.
  const next = await rememberScopedTarget(store, {
    platform: target.platform, origin: target.origin, scope: UNRESOLVED_SCOPE, at: Date.now(),
  });
  const existingOrg = next.find(
    (t) => t.platform === target.platform && isClaudeOrgId(t.scope),
  );
  if (existingOrg) return existingOrg.scope;
  const resolver = scopeResolverFor(target.platform);
  if (!resolver) return UNRESOLVED_SCOPE;
  if (!(await scopeRetryDue(store, target.platform, UNRESOLVED_SCOPE, Date.now()))) {
    return UNRESOLVED_SCOPE;
  }
  const resolved = await resolver(null, target.origin);
  if (!resolved.ok) {
    await recordBackfillHalt(store, {
      platform: target.platform, scope: UNRESOLVED_SCOPE, reason: resolved.halt, detail: resolved.detail,
    });
    // The engine now finds the halt record and stops by name, issuing nothing.
    return UNRESOLVED_SCOPE;
  }
  await rememberScopedTarget(store, {
    platform: target.platform, origin: target.origin, scope: resolved.org, at: Date.now(),
  });
  return resolved.org;
}

async function runAlarmTickBody(): Promise<TickResult> {
  const store = browserLocalStore();

  // 🔴 W2 · Every alarm wake sends the outbox first (the first of task 5's two
  //    occasions). It is ordered before the backfill leg: what is sitting in the
  //    spool is the conversation the user was looking at at the time, whereas the
  //    backfill leg is filling in history — get the one in hand out first. The
  //    two legs do not affect each other.
  const drain = await drainSafely();
  markOutboxDrain(drain);
  await syncOutboxAlarmSafely();
  await refreshBadgeSafely();

  const targets = await loadTargets(store);

  // 🔴 W36/W36b · The storage layout moves **here**, before any gate decides
  //    whether this tick may fetch anything, and the sweep scans `storage.local`
  //    rather than the target registry. The migration used to be reachable only
  //    from a run that was about to make a request, so a scope whose ticks were
  //    all blocked (no tab open — the ordinary state of a laptop) never moved at
  //    all; and W36's version walked `loadTargets()`, so a pre-W18 record whose
  //    scope is not registered was visited by nothing even here. See
  //    migrateLegacyScopes.
  // 🔴 W47 · **The other half of the same preflight: a record this build cannot
  //    read is named here, before the gates, because the gates can stop the tick
  //    before anything ever opens the ledger.**
  //
  //    `openLedger`'s refusal is carried into the run's report, and the run only
  //    happens on a tick that got past every gate — switch on, host answering, a
  //    registered target and an open platform page. With the tabs closed, which is
  //    the ordinary state of a laptop, no tick ever reaches it: the trace says
  //    `no-http-port` and the unreadable record is named by nothing. That is
  //    exactly the state the W18 comment forbids reading as "no refusal happened",
  //    and it is why this is looked for in `storage.local` rather than in a run.
  //    See findUnreadableState for what it costs and what it deliberately does not
  //    write.
  //
  //    The two preflights can both find something and the trace has one field, so
  //    the migration's refusal wins and the probe is skipped (`??` short-circuits,
  //    which also makes a tick whose layout has not moved no more expensive than it
  //    was): the migration's refusal is a fact about a record **this build was asked
  //    to move**, while the probe's is about one it merely read.
  const migration = await migrateLegacyScopes(store);
  const preflightRefusal: LedgerRefusal | null =
    migration.refusal ?? await findUnreadableState(store);

  if (targets.length === 0) {
    // No targets at all ⇒ the user has never been captured on a supported
    // platform. The gates are still run once, so that the popup's lastTickReason
    // can tell the truth.
    const blocked = await tickBlockReason({
      hasStore: store !== null,
      isEnabled: () => isBackfillEnabled(store),
      isHostPaused: () => isBackfillHostPaused(store),
      // 🔴 C30: both of these are **facts**. This used to hardcode hasHttp to
      //    false and then fall back to 'no-http-port', so "the channel is
      //    perfectly connected, there simply are no targets" was reported as a
      //    broken port — and whoever diagnosed it went off chasing the port,
      //    which was never broken.
      hasHttp: false,
      hasTargets: false,
    });
    lastTick = { ran: false, reason: blocked ?? 'no-targets', report: null };
    await recordAlarmTick(store, lastTick, 0, preflightRefusal, null);
    return lastTick;
  }

  let last: TickResult = { ran: false, reason: 'no-http-port', report: null };
  /**
   * 🔴 W51 · The recovery sweep runs **at most once per tick**, and only when a
   *    registered target is about to concede `no-http-port`. A tick that already
   *    has a channel, or that was blocked at an earlier gate, leaves this `null`
   *    — that is "never swept", and it must stay distinguishable from a sweep
   *    that looked and found nothing.
   */
  let tabSweep: TabSweepReport | null = null;
  for (const target of targets) {
    const scope = await resolveScopeForTick(store, target);
    const tickOne = (http: Awaited<ReturnType<typeof resolveHttpPort>>): Promise<TickResult> =>
      tickBackfill({
        platform: target.platform,
        origin: target.origin,
        scope,
        store,
        http,
        // 🔴 C20: the alarm's kick must return too (both heartbeats share one exit,
        //    and one of them reporting while the other does not is not acceptable).
        sink: (c) => deliverBackfillItem(c),
        ...(backfillPaceOverride ?? {}),
      });
    let result = await tickOne(await resolveHttpPort(target.origin));
    if (result.reason === 'no-http-port' && tabSweep === null) {
      tabSweep = await recoverUnregisteredTabs();
      // Retry this target only by aiming at a row the sweep just registered
      // of *this* origin. Walking pickLiveTab again would re-strike the
      // silent tab this tick already counted (and, if the recovered tab's
      // re-ping failed, spend both TAB_PING_MISSES_BEFORE_FORGET strikes in
      // one wake). Rows the sweep did not touch keep the miss they already
      // took; pickLiveTab's order and two-strike rule are unchanged.
      if (tabSweep.looked) {
        const recoveredId = tabSweep.recovered.find((row) => row.origin === target.origin)?.tabId;
        if (recoveredId !== undefined) {
          result = await tickOne(await resolveHttpPort(target.origin, recoveredId));
        }
      }
    }
    last = result;
    lastTick = result;
    // If it ran, stop. If it was blocked by the switch / storage / a host pause,
    // there is no point trying another target — the conclusion would be the same.
    if (result.reason !== 'no-http-port') break;
  }
  await recordAlarmTick(store, last, targets.length, preflightRefusal, tabSweep);
  return last;
}

/** What the watchdog decided. `idle` is the healthy case and must be the common one. */
export type WatchdogOutcome = 'idle' | 'rearmed' | 'disabled';

/**
 * 🔴 W16 · **The safety net's handler.** It is not a second heartbeat — it is a
 * watchdog, and in the healthy case it does nothing at all.
 *
 * The failure it exists for: the tick alarm is a one-shot, so the chain only
 * continues because the end of each tick arms the next one. A service worker
 * killed inside a tick never reaches that line, and a one-shot alarm that fired
 * is already gone — so without this, nothing would ever wake the leg again.
 *
 * It is deliberately cheap: it reads the switch and asks the browser whether the
 * jittered alarm exists. No storage snapshot, no target lookup, no tab ping, no
 * request. If the chain is armed it returns `idle` and the tick count for the
 * day is exactly what the jittered alarm decided — the cadence stays irregular.
 * If the chain is broken it runs one normal tick, and that tick arms the next
 * one, so a single watchdog fire is enough to restore the whole chain.
 */
export async function runBackfillWatchdog(): Promise<WatchdogOutcome> {
  const store = browserLocalStore();
  // With the switch off there is nothing to watch: no consent, no periodic
  // behaviour, not even a wake-up that reads anything else.
  if (!(await isBackfillEnabled(store))) return 'disabled';
  if (await isBackfillChainArmed(alarmsApi())) return 'idle';
  console.warn(
    '[chat-stasher] backfill watchdog: the jittered tick alarm was not armed'
    + ' (a worker killed before re-arming) — restoring the chain',
  );
  await runAlarmTick();
  return 'rearmed';
}

/** The gate's "are we paused right now" — read-only, no recovery attempt (recovery is tickBackfill's job). */
async function isBackfillHostPaused(store: ReturnType<typeof browserLocalStore>): Promise<boolean> {
  return (await loadHostPause(store)) !== null;
}

/**
 * The conclusion of the most recent outbox drain (in-memory; for tests and
 * diagnosis only).
 * 🔴 Like the backfill leg's lastTick: it is gone once the SW is reclaimed — what
 *    is in the outbox is only ever decided by the outbox itself, and nothing here
 *    carries any truth.
 */
let lastDrain: Awaited<ReturnType<typeof drainOutbox>> | null = null;

/** The drain started by the most recent outbox alarm, so tests can await it. */
let pendingOutboxAlarmDrain: Promise<unknown> | null = null;

export function outboxAlarmSettled(): Promise<unknown> {
  return pendingOutboxAlarmDrain ?? Promise.resolve(null);
}

function markOutboxDrain(report: Awaited<ReturnType<typeof drainOutbox>>): void {
  lastDrain = report;
  if (report.attempted > 0 || report.stoppedBy === 'host-unavailable') {
    console.log(
      `[chat-stasher] outbox drain: attempted ${report.attempted},`
      + ` delivered ${report.delivered}, rejected ${report.rejected}, stopped by ${report.stoppedBy}`,
    );
  }
}

export function lastOutboxDrain(): Awaited<ReturnType<typeof drainOutbox>> | null {
  return lastDrain;
}

/**
 * 🔴 C30 · Write the conclusion of this alarm tick to storage.
 *
 * Why it must be persisted rather than left in the lastTick variable: an MV3 SW
 * is reclaimed as soon as this tick finishes, and when the user opens the popup a
 * while later nothing is left in memory — which is exactly what a real machine
 * showed: the alarm kept waking and kept doing nothing, and nowhere could you find
 * out that it had ever woken.
 * A failed write only warns; leaving a trace must never become a new reason to
 * block this leg.
 */
async function recordAlarmTick(
  store: ReturnType<typeof browserLocalStore>,
  result: TickResult,
  targets: number,
  /**
   * 🔴 W36 · A refusal reached **before** the run could start — the pre-W18
   * migration's preflight (W36b) or a current-layout record this build cannot read
   * (W47, `findUnreadableState`). When the run did happen, its own report is the
   * more precise fact and wins; when the tick was blocked at a gate, this is the
   * only thing that knows why nothing moved — which is exactly the state the first
   * acceptance found (a v1 record, no v2 key, and a trace that said only `ran`).
   */
  preflightRefusal: LedgerRefusal | null = null,
  /**
   * 🔴 W51 · The tab-registry recovery sweep, or `null` when this tick never
   *    swept. Written as `tabSweep` on the trace so a later reader can tell
   *    "we looked and found nothing" from "we never looked".
   */
  tabSweep: TabSweepReport | null = null,
): Promise<void> {
  const halt = result.report?.halted ?? null;
  await saveLastTick(store, {
    at: Date.now(),
    ran: result.ran,
    reason: result.reason,
    targets,
    /**
     * 🔴 W47 · **Every path that writes a trace names how it ended.**
     *
     * `report?.stopped` is the run's own stop; the fallback is the tick's own
     * named outcome, for the paths that never reached a run (blocked at a gate,
     * or already running). Both are existing members of closed sets — `StopReason`
     * in lib/backfill/types.ts, `TickReason` in lib/backfill/schedule.ts — and the
     * field is read as "how this ended, at whichever layer ended it".
     *
     * Why it may not stay null here: the null was the *first* thing a person read
     * on a real machine (`{ran: false, reason: 'no-http-port', stopped: null,
     * halted: null}`) and it says nothing that `reason` does not — but the whole
     * audience of this record is someone asking "why is nothing moving", and the
     * field that answers it falls back to the tick's own answer rather than to a
     * blank. Nothing new is named: `stopped` for a gate-blocked tick is the same
     * value `reason` already carries.
     */
    stopped: result.report?.stopped ?? result.reason,
    /**
     * 🔴 R47 · The preflight is attached **only when no run happened**. It walks
     *    every `cs_backfill_v2:*` key, not the target this tick used, so a single
     *    unreadable record left over from another scope would otherwise ride along
     *    on a tick that ran and archived — and the popup would say "that tick
     *    stopped before it could finish" under a `ran: true` head, for every tick,
     *    until someone removed the stale key by hand.
     */
    halted: halt?.reason ?? (result.ran ? null : preflightRefusal?.reason) ?? null,
    detail: halt?.detail ?? (result.ran ? null : preflightRefusal?.detail) ?? null,
    tabSweep: persistSweep(tabSweep),
  });
}

/**
 * 🔴 R47 · An origin, or nothing. A message that named something which is not a
 * URL named no origin, and the decline record says so rather than storing a path.
 */
function originOf(value: string): string | null {
  try {
    return new URL(value).origin;
  } catch {
    return null;
  }
}

function cancelledIdLike(id: string | null): boolean {
  if (!id) return true;
  return id.length < 8 || id === 'unknown';
}

/**
 * Whatever state the switch is in, the alarm should be in. Every SW wake re-syncs the two.
 *
 * 🔴 W16 · This is the **second half** of "the chain can never stay broken".
 *    Since the tick alarm is a one-shot, it is absent whenever it has fired and
 *    not yet been re-armed — and this function runs on every single service
 *    worker wake (`runtime.onStartup`, every live capture via `kickBackfill`'s
 *    SW being alive, every switch change, and the startup setup below). Each
 *    time it finds the chain missing it arms a fresh draw. So in practice a
 *    broken chain is repaired by the next ordinary wake, long before the
 *    watchdog's one hour is up; the watchdog covers the case where nothing else
 *    wakes the worker at all.
 */
export async function syncAlarmWithSwitch(): Promise<string> {
  const enabled = await isBackfillEnabled(browserLocalStore());
  return syncBackfillAlarm(alarmsApi(), enabled, backfillRandom());
}

type StorageChange = { newValue?: unknown };

function addBackfillSwitchListener(): void {
  const storage = (browser as unknown as {
    storage?: {
      onChanged?: {
        addListener(fn: (changes: Record<string, StorageChange>, areaName: string) => void): void;
      };
    };
  }).storage;
  storage?.onChanged?.addListener((changes, areaName) => {
    if (areaName !== 'local' || !(BACKFILL_ENABLED_KEY in changes)) return;
    // storage.onChanged fires after the write is committed. Re-read through the
    // same gate as startup so remove/non-boolean values also fail closed.
    void syncAlarmWithSwitch().catch((err) => {
      console.warn('[chat-stasher] backfill alarm switch sync failed', (err as Error).message);
    });
  });
}

/** The detached async setup started by main(); resolved once it has finished. */
let backgroundSetup: Promise<void> = Promise.resolve();

/** Wait for main()'s async setup (locale, badge, alarms). main() itself stays synchronous. */
export function backgroundSetupSettled(): Promise<void> {
  return backgroundSetup;
}

// 🔴 main() is synchronous and registers every listener before any await: MV3
//    wakes a reclaimed worker with the event itself, and WXT warns that an async
//    main is unsupported. The async setup runs detached at the end.
export default defineBackground(() => {
  browser.runtime.onMessage.addListener(
    (message: { type?: string; payload?: CapturedFetch }, sender, sendResponse) => {
      // C18: the popup asks, when it opens, "is the fetch channel connected".
      // Since C19 that answer requires pinging a tab on the spot ⇒ it is async,
      // and it still returns true.
      // 🔴 W2: the same answer also carries "is the host there" — background asks
      //    the host and writes the conclusion down, and the popup only displays
      //    the facts it was handed (it has not one line of probing code of its own).
      if (message?.type === POPUP_STATUS_MESSAGE) {
        popupHostStatus()
          .then(sendResponse)
          // If it cannot be answered, answer in the most conservative direction;
          // never let the popup show it as "running".
          .catch(() => sendResponse({ transportWired: false, lastTickReason: null, nativeHost: null }));
        return true;
      }
      // C19: a content script checking in. The browser fills the tab id in on
      // `sender` (no 'tabs' permission needed), and once it is in storage the
      // alarm knows who to fetch through when it wakes.
      // 🔴 C33: the kick from the popup's "start backfilling this platform" button.
      //    It answers only **after** the write — an ok has to mean "that row really
      //    is in the registry now", otherwise the popup's immediate repaint reads a
      //    registry that has not landed yet and it looks like nothing happened.
      if (message?.type === POPUP_START_BACKFILL_MESSAGE) {
        registerBackfillTargetHere()
          .then(sendResponse)
          .catch((err: Error) => {
            console.warn('[chat-stasher] backfill start-here failed', err.message);
            sendResponse({ ok: false, reason: 'no-store' });
          });
        return true;
      }
      if (isTabHello(message)) {
        const tabId = (sender as { tab?: { id?: number } })?.tab?.id;
        if (typeof tabId !== 'number') {
          sendResponse({ ok: false, error: 'no tab id on sender' });
          return true;
        }
        // 🔴 Answer only **after** the write: an ok has to mean "the alarm can find
        //    you now", otherwise the alarm or status query right behind it reads a
        //    registry that has not landed yet.
        rememberTab(browserLocalStore(), { tabId, origin: message.origin, at: Date.now() })
          .then(() => sendResponse({ ok: true }))
          .catch((err: Error) => {
            console.warn('[chat-stasher] tab registry write failed', err.message);
            sendResponse({ ok: false, error: err.message });
          });
        return true;
      }
      /**
       * 🔴 W47 · **A message that claims to be a hook report and is not one this
       * build can read.**
       *
       * It fell through to the `chat-captured` test below and was dropped there
       * without a word — which is the same silence as a report that never arrived.
       * The condition is deliberately narrower than "any message that failed a
       * guard": only a message that **says** it is a hook report
       * (`type === HOOK_STATUS_MESSAGE`) and then fails that report's own guard is
       * counted here. Every other message type on this listener is somebody else's
       * traffic, and recording those would be recording a fact about a request that
       * was never made.
       */
      if (message?.type === HOOK_STATUS_MESSAGE && !isHookStatusMessage(message)) {
        recordHookDecline(browserLocalStore(), {
          reason: HOOK_DECLINE_UNREADABLE_MESSAGE,
          // Deliberately nothing else: a message this build cannot read is a message
          // whose origin and reason are exactly the parts that did not check out,
          // and writing either into a record a person reads would be inventing it.
          at: Date.now(),
        })
          .then(() => sendResponse({ ok: false, error: 'unreadable hook report' }))
          .catch((err: Error) => {
            console.warn('[chat-stasher] hook decline record write failed', err.message);
            sendResponse({ ok: false, error: 'unreadable hook report' });
          });
        return true;
      }
      if (isHookStatusMessage(message)) {
        /**
         * 🔴 W43 · **A page telling us what it observed about its own hook.**
         *
         * Two checks before a single byte is written, and both are about not
         * trusting the message. The **origin** must be one of the eight in the
         * platform table: the sender is a content script of this extension, but
         * the record this produces is shown to the user, and a record naming an
         * origin the extension does not even inject into would be a sentence about
         * somebody else's page. The **reason** is checked by the guard
         * (`isHookStatusMessage`) against the closed set, because the page world
         * can post anything on its own window and the bridge relays what that
         * guard accepts.
         *
         * 🔴 The reply says only that the write happened; nothing is claimed about
         *    the page. `reason: null` is a top frame whose hook verified, which
         *    clears the origin's record — the one way it is ever cleared
         *    (`lib/hook-status.ts`). A child frame never reaches this branch: the
         *    bridge drops its observation (`entrypoints/dw-bridge.content.ts`).
         *
         * 🔴 W47 · **A refusal here is a fact, and it is written down.** From
         *    outside, "background refused this report" and "nothing ever arrived"
         *    are the same state, and they are two different facts.
         *    `recordHookDecline` writes the refusal down; the page's own record is
         *    still not written, because a record naming an origin this extension
         *    does not inject into would be a sentence about somebody else's page.
         */
        const platform = PLATFORMS.find((row) => row.origins.includes(message.origin));
        if (!platform) {
          recordHookDecline(browserLocalStore(), {
            reason: HOOK_DECLINE_NOT_A_PLATFORM_ORIGIN,
            // 🔴 R47 · `isHookStatusMessage` proves only "a non-empty string". What
            //    is stored and shown must be an origin, so it is normalised here;
            //    a value that is not a URL is recorded as `null`, which the record
            //    already means as "it named no usable origin". The guard itself is
            //    left alone: widening what counts as a valid message is not this
            //    change.
            origin: originOf(message.origin),
            observation: message.reason,
            at: message.observedAt,
          })
            .then(() => sendResponse({ ok: false, error: 'unknown origin' }))
            .catch((err: Error) => {
              console.warn('[chat-stasher] hook decline record write failed', err.message);
              sendResponse({ ok: false, error: 'unknown origin' });
            });
          return true;
        }
        recordHookStatus(browserLocalStore(), {
          origin: message.origin,
          platform: platform.id,
          reason: message.reason,
          at: message.observedAt,
        })
          .then(() => sendResponse({ ok: true }))
          .catch((err: Error) => {
            console.warn('[chat-stasher] hook status write failed', err.message);
            sendResponse({ ok: false, error: err.message });
          });
        return true;
      }
      if (message?.type !== 'chat-captured' || !message.payload) return;
      const payload = message.payload;
      const senderTabId = (sender as { tab?: { id?: number } })?.tab?.id;
      handleCaptured(payload)
        .then((result) => {
          // C13: this live-leg kick also wakes the backfill leg. Fire-and-forget —
          // backfilling is slow work and must never block sendResponse or slow the
          // write-down of the user's current conversation.
          pendingTick = kickBackfill(payload, senderTabId).catch((err) => {
            console.warn('[chat-stasher] backfill tick failed', (err as Error).message);
            return null;
          });
          // Privacy rule: never the conversation content — ids/bytes only.
          if (result.status === 'unchanged') {
            // 🔴 saved:true, but **nothing was acknowledged just now**: this exact
            //    content was acked earlier, so it was not sent again. Saying
            //    "acknowledged" here would report an ack that did not happen.
            console.log(
              `[chat-stasher] unchanged since its last delivery, not sent again`
              + ` — ${result.bytes ?? 0} bytes as ${result.finalName}`,
            );
          } else if (result.saved) {
            console.log(`[chat-stasher] acknowledged ${result.bytes} bytes as ${result.finalName}`);
          } else {
            console.log(
              `[chat-stasher] not delivered yet (${result.status}: ${result.reason ?? 'no reason given'})`
              + ` — ${result.bytes ?? 0} bytes`,
            );
          }
          // 🔴 `ok` is still an alias of `saved`, but the payload now carries a
          //    distinguishable `status`: the caller (the content script) can no
          //    longer be told "no ack but it counts as success".
          sendResponse({ ok: result.saved, ...result });
        })
        .catch((err) => {
          console.error('[chat-stasher] handleCaptured failed', (err as Error).message);
          sendResponse({ ok: false, error: (err as Error).message });
        });
      // MV3: async sendResponse requires returning true to keep the channel open.
      return true;
    },
  );

  // 🔴 C19 · The alarm listener must be registered **synchronously** at the SW's
  //    top level: under MV3 a reclaimed SW is woken by the event itself, and
  //    registering late misses that wake-up.
  const alarms = (browser as unknown as {
    alarms?: AlarmsApi & { onAlarm?: { addListener(fn: (a: { name?: string }) => void): void } };
  }).alarms;
  alarms?.onAlarm?.addListener((alarm) => {
    if (alarm?.name === OUTBOX_ALARM_NAME) {
      // §10: outbox retries do not depend on the backfill switch.
      pendingOutboxAlarmDrain = drainSafely().then(async (report) => {
        markOutboxDrain(report);
        await refreshBadgeSafely();
        await syncOutboxAlarmSafely();
        return report;
      });
      return;
    }
    // 🔴 W16 · The watchdog's wake is **not** a tick. It decides for itself
    //    whether the chain is broken; on a healthy leg it returns without doing
    //    anything, so the backfill cadence stays the jittered one and only the
    //    jittered one. It must therefore not be routed through runAlarmTick
    //    unconditionally — that would be a fixed hourly tick, i.e. exactly the
    //    periodic behaviour this change removed.
    if (alarm?.name === BACKFILL_SAFETY_ALARM_NAME) {
      pendingTick = runBackfillWatchdog().catch((err) => {
        console.warn('[chat-stasher] backfill watchdog failed', (err as Error).message);
        return null;
      });
      return;
    }
    if (alarm?.name !== BACKFILL_ALARM_NAME) return;
    pendingTick = runAlarmTick().catch((err) => {
      console.warn('[chat-stasher] backfill alarm tick failed', (err as Error).message);
      return null;
    });
  });

  // Popup changes the persisted switch from another extension context. Keep
  // the alarm lifecycle closed immediately instead of waiting for a later SW
  // wake-up. The listener itself is synchronous, as required by MV3.
  addBackfillSwitchListener();

  // Registered before any await, like every other listener (see the note on main()).
  browser.runtime.onStartup.addListener(() => {
    void refreshBadge();
    void syncAlarmWithSwitch().catch(() => { /* already logged on the path below */ });
    void syncOutboxAlarmSafely();
  });

  backgroundSetup = (async () => {
    // 🔴 Load the language the user chose before anything paints text. The badge
    //    tooltip is set from this worker, so the overlay has to be initialised here
    //    too — otherwise the tooltip would stay in the browser's language while the
    //    popup follows the setting. A failure is only logged: falling back to
    //    browser.i18n is the pre-existing behaviour, not a reason to skip the badge.
    await initUiLocale().catch((err) => {
      console.warn('[chat-stasher] ui locale init failed', (err as Error).message);
    });

    // Every SW wake (fresh start AND runtime.onStartup) re-asserts the badge's
    // truth, so a dead-worker leftover badge gets cleared once 5 min pass.
    void refreshBadge();
    // The switch is persistent, so the alarm should be too. Every SW wake re-syncs:
    // on ⇒ make sure an alarm exists; off ⇒ make sure none does — 🔴 with the
    // default (off) this only ever clears, and never creates one out of nowhere.
    await syncAlarmWithSwitch().catch((err) => {
      console.warn('[chat-stasher] backfill alarm sync failed', (err as Error).message);
    });
    // The outbox timer follows the outbox, not the switch (§10).
    await syncOutboxAlarmSafely();

    console.log(
      '[chat-stasher] background ready: captures are queued in the outbox and delivered'
      + ' to the chat-stasher native host; nothing is ever downloaded',
    );
  })();
});
