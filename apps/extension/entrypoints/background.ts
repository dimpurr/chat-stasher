import {
  extractIdentity,
  extractSessionId,
  SCHEMA,
  pathSafeSessionId,
  findPlatformForUrl,
  getPlatformByOrigin,
  type CapturedFetch,
  type InboxBundle,
} from '../lib/contract';
import { refreshBadge } from '../lib/badge';
import { browserLocalStore } from '../lib/backfill/store';
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
import type { BackfillOptions, HttpPort } from '../lib/backfill/engine';
import {
  BACKFILL_ALARM_NAME,
  loadTargets,
  rememberTarget,
  saveLastTick,
  syncBackfillAlarm,
  type AlarmsApi,
} from '../lib/backfill/alarm';
import {
  BACKFILL_PING_MESSAGE,
  isTabHello,
  pickLiveTab,
  rememberTab,
  tabHttpPort,
  type TabSend,
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
/** Test seam: inject a fake clock / a custom pacing so tests do not really sleep 20 seconds. Always null in production. */
let backfillPaceOverride: { pace?: BackfillOptions['pace']; clock?: BackfillOptions['clock'] } | null = null;

export function configureBackfillTransport(http: HttpPort | null): void {
  backfillTransport = http;
}

export function configureBackfillPace(
  override: { pace?: BackfillOptions['pace']; clock?: BackfillOptions['clock'] } | null,
): void {
  backfillPaceOverride = override;
}

/** The most recent tick's result (for tests and diagnosis, and for C18's popup). */
export function lastBackfillTick(): TickResult | null {
  return lastTick;
}

function alarmsApi(): AlarmsApi | null {
  return (browser as unknown as { alarms?: AlarmsApi }).alarms ?? null;
}

function tabsApi(): { sendMessage: TabSend } | null {
  const tabs = (browser as unknown as { tabs?: { sendMessage?: TabSend } }).tabs;
  // 🔴 tabs.sendMessage does not need the 'tabs' permission ('tabs' only governs
  //    sensitive fields like url/title), and we only message **our own injected
  //    content script**. The matches need not change a character.
  return tabs && typeof tabs.sendMessage === 'function'
    ? { sendMessage: (id, msg) => tabs.sendMessage!(id, msg) }
    : null;
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
}> {
  if (backfillTransport) return { wired: true, target: null };
  const tabs = tabsApi();
  if (!tabs) return { wired: false, target: null };
  const live = await pickLiveTab(browserLocalStore(), null, (id) =>
    tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
  if (!live) return { wired: false, target: null };
  const row = getPlatformByOrigin(live.origin);
  // The origin is not in the platform table ⇒ we cannot answer "which platform is
  // this" ⇒ say null plainly, never guess one.
  return { wired: true, target: row ? { platform: row.id, origin: live.origin } : null };
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
 * 🔴 What about scope (the account identifier): **it records 'default'**, and the
 * reason is written here rather than quietly inventing a value.
 *  · The channel hop (the content script's ping) brings back the origin only,
 *    **no account information at all** — getting a real account would mean
 *    reading a response body out of the page, which is a guess, and this change
 *    does not do it;
 *  · 'default' is not a new invention: it is this repository's existing spelling
 *    for "the account cannot be told" (entrypoints/background.ts:303's
 *    `identity.value || 'default'`, lib/popup-view.ts's emptyStateFor,
 *    lib/contract.ts's IdentityLevel 'default'), and it means, literally,
 *    "the identity is unreliable", not an impersonation of some specific account;
 *  · fetching still uses the login of the user's own page, so **what comes back
 *    really is their own history**; scope is only the partition key of the debt
 *    ledger, and writing 'default' does not pull anyone else's material in here.
 *  · The cost (written down honestly): a later real capture registers a second
 *    target carrying the real account, so there will be two debt sets under the
 *    same platform and the same conversations may be cleared once each.
 *    File names are platform-sessionId and writes overwrite ⇒ no cross-talk and
 *    no erasing each other; the only extra cost is duplicate fetching and writing.
 *    Against "backfill never starts at all", that is worth paying.
 */
export async function registerBackfillTargetHere(): Promise<
  | { ok: true; target: { platform: string; origin: string; scope: string } }
  | { ok: false; reason: 'no-store' | 'no-live-transport' | 'origin-not-a-platform' }
> {
  const store = browserLocalStore();
  if (!store) return { ok: false, reason: 'no-store' };
  const live = await liveTransport();
  if (!live.wired) return { ok: false, reason: 'no-live-transport' };
  if (!live.target) return { ok: false, reason: 'origin-not-a-platform' };
  // 🔴 scope = 'default': see the block above. That is an existing convention, not a new value.
  const target = { platform: live.target.platform, origin: live.target.origin, scope: 'default' };
  await rememberTarget(store, { ...target, at: Date.now() });
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
    await rememberTarget(store, { ...target, at: Date.now() });
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
 */
export async function runAlarmTick(): Promise<TickResult> {
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
    await recordAlarmTick(store, lastTick, 0);
    return lastTick;
  }

  let last: TickResult = { ran: false, reason: 'no-http-port', report: null };
  for (const target of targets) {
    const result = await tickBackfill({
      platform: target.platform,
      origin: target.origin,
      scope: target.scope,
      store,
      http: await resolveHttpPort(target.origin),
      // 🔴 C20: the alarm's kick must return too (both heartbeats share one exit,
      //    and one of them reporting while the other does not is not acceptable).
      sink: (c) => deliverBackfillItem(c),
      ...(backfillPaceOverride ?? {}),
    });
    last = result;
    lastTick = result;
    // If it ran, stop. If it was blocked by the switch / storage / a host pause,
    // there is no point trying another target — the conclusion would be the same.
    if (result.reason !== 'no-http-port') break;
  }
  await recordAlarmTick(store, last, targets.length);
  return last;
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
): Promise<void> {
  await saveLastTick(store, {
    at: Date.now(),
    ran: result.ran,
    reason: result.reason,
    targets,
  });
}

function cancelledIdLike(id: string | null): boolean {
  if (!id) return true;
  return id.length < 8 || id === 'unknown';
}

/** Whatever state the switch is in, the alarm should be in. Every SW wake re-syncs the two. */
export async function syncAlarmWithSwitch(): Promise<string> {
  const enabled = await isBackfillEnabled(browserLocalStore());
  return syncBackfillAlarm(alarmsApi(), enabled);
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
