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
/**
 * 实时腿一次捕获的结局。
 *
 * 🔴 W2 · **唯一算「存下来了」的取值是 `saved:true`，而它只在收到匹配 ack 时出现。**
 *    已入队但还没被确认是另一个可区分的结局（`status:'queued'`），
 *    它既不是成功也不是失败 —— 角标按「待送数」计它。
 */
export interface HandledResult {
  /** 🔴 true 当且仅当这一条 payload 收到了匹配的 `ack`（§1）。 */
  saved: boolean;
  /** 四种互斥的结局，每一种都必须说得出口。 */
  status: 'delivered' | 'queued' | 'rejected' | 'refused';
  reason?: string;
  /** 被拒时的 `nack` kind（§6.3）。 */
  kind?: string;
  /**
   * 🔴 C20：落盘时【实际用来命名】的那个身份。
   * 根因是「同一个身份被表达了两次」—— 欠账键来自列表接口的 items[].id，
   * 文件名来自「用正则从 URL 里再抠一次」，中间没有任何一致性校验。
   * 把这一位如实报出来，回溯腿才有可能当场对一次账（见 engine.ts 的 sinkVerdict）。
   * saved:false 时为 undefined（根本没有命名过）。
   */
  sessionId?: string;
  /** 这条 payload 在本机 host 那边的名字（§6.2 的 `name`，即分片的 source_file）。 */
  finalName?: string;
  bytes?: number;
  /**
   * 🔴 只有一条通道了。这个字段留着是为了让「没有降级通道」这件事在类型上
   * 显式可见：`downloads` 这个取值连同整条自动下载通道已经被删除。
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
 * 🔴 C21 · **这条链路上唯一一处决定「这条会话是谁」的地方。**
 *
 * 顺序不是偏好，是根因治理本身：
 *  1. `captured.sessionId` —— 上游【已经知道】身份时一路带下来的权威值
 *     （回溯腿：欠账键 = 列表接口的 items[].id，见 lib/backfill/engine.ts）。
 *     有它就【绝不再推导】—— 第二次表达就是在这里消失的。
 *  2. 没有它才回落到 extractSessionId（实时腿：身份只存在于 URL 里，别无来源）。
 *
 * 页面来的载荷永远走不到第 1 支：lib/contract.ts 的 isCapturedFetchShape
 * 对带 sessionId 的页面载荷【存在即拒收】。
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

  // 🔴 ADR-025 §10 · **write-ahead**：先落进发件箱，再做任何投递尝试。
  //    在这两行之间 SW 被杀掉，会话仍然在盘上等着，下一次排空会把它送出去。
  const queued: EnqueueResult = await enqueue(name, payload);
  if (!queued.accepted || !queued.sha256) {
    // 存不进发件箱 ⇒ 不投递。投出去的东西发件箱不知道，那正好是 write-ahead
    // 要防的那件事；宁可如实报一次拒收，也不悄悄少一层保障。
    return {
      saved: false,
      status: 'refused',
      reason: queued.reason ?? 'outbox-unavailable',
      finalName: name,
      bytes,
      sessionId,
    };
  }

  // 入队后立刻尝试一次排空（任务 5 的第二个时机；第一个是闹钟心跳）。
  const drained = await drainSafely();
  await syncOutboxAlarmSafely();

  const lookup = await getEntry(queued.sha256);
  await refreshBadgeSafely();

  if (!lookup.ok) {
    // 🔴 「读不出来」不是「送达了」也不是「没送达」。这一条确实已经写进发件箱了，
    //    但我们答不上它现在是什么状态 —— 那就照实说答不上来，绝不猜成 saved。
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
    // 🔴 只有匹配的 ack 才会删条目（§1），所以「查不到」= 这一条真的落盘了。
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

/** 名字与载荷的唯一构造点。实时腿和回溯腿【共用】它，落盘逻辑不分叉。 */
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
  // 🔴 C21 · 命名是【恒等映射】，不是「换掉不安全字符」：
  //    sanitizePathSegment 是多对一的（'a b' 与 'a/b' 同名），以前下载是 overwrite ⇒
  //    两条不同的会话能互相抹掉。起不出安全名字就当场收手、留痕，绝不硬塞。
  const named = pathSafeSessionId(bundle.sessionId);
  if (named === null) {
    return { ok: false, reason: 'session id is not usable as a file name (refused, not collapsed)' };
  }
  const name = `${bundle.platform}-${named}.json`;
  // §6.2 的 name 规则。我们的构造器本来就只会产出合法的名字，这里是防御性的
  // 最后一道：宁可当场拒收，也不把一个主机明确会 nack 的名字发出去。
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

/** 排空：绝不允许它把「用户当前这条对话」这条路带下水。 */
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
 * 角标是装饰性的：它自己出错不许影响任何一条真实路径。
 * 🔴 但它**要被 await**：不等它，调用方拿到的就只是"角标即将被重画"，
 *    而角标说的正是「发件箱现在有几条待送」—— 那个数字必须与本次结果同拍。
 */
async function refreshBadgeSafely(): Promise<void> {
  try {
    await refreshBadge();
  } catch (err) {
    console.warn('[chat-stasher] badge update failed', (err as Error).message);
  }
}

/**
 * 🔴 W2 · **回溯腿的出口。它不进发件箱。**（§10）
 *
 * 理由：这条会话在平台上还在，欠账才是它该待的地方 —— 排队一份 payload 只会
 * 让同一个会话在「欠账」和「发件箱」两本账上各存在一次。
 *
 * 三种结局，一种都不许混：
 *  · 匹配 ack          ⇒ saved:true，engine 清账；
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
  // 🔴 出口够不着：欠账保持、暂停留痕（§10「visible reason」）。
  await setHostPause(browserLocalStore(), {
    reason: HOST_UNAVAILABLE,
    at: Date.now(),
    detail: result.detail ? `${result.reason}: ${result.detail}` : result.reason,
  });
  return { saved: false, reason: HOST_UNAVAILABLE, retryLater: true };
}

/**
 * Popup 打开时问一句「主机在不在、往哪儿写」。
 * 探测窗口短（HELLO_PROBE_TIMEOUT_MS）：Popup 是界面，不许被一个卡住的主机吊住；
 * 投递路径用的仍然是 §2 规定的 60 秒。结论会被写进 storage.local —— 写下来
 * 才是「下一次打开 Popup 还看得见」的唯一办法。
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
// C13 → C19 · 回溯腿的接线
//
// C13 的触发点是【实时腿的 onMessage】，理由是 MV3 里 SW 平时是死的、而实时腿
// 那条消息里现成带着 platform/origin/账号 scope。那些理由今天仍然成立，所以
// 那一脚【保留】。但它有一个致命推论：
// 🔴 一个装了之后再也不打开那个网站的用户，永远不会有第二条实时捕获来踢它，
//    历史就永远补不完。
// ⇒ C19 加了第二个心跳：chrome.alarms（见 lib/backfill/alarm.ts，周期与理由都在
//   那里）。两个心跳走的是同一个 tickBackfill，闸门顺序完全一致。
//   闹钟醒来时 SW 是全新的、什么都不知道 —— 所以实时腿踢那一脚时会顺手把目标
//   记进 storage（rememberTarget），闹钟直接读它，一个字都不用猜。
// ---------------------------------------------------------------------------

/**
 * 🔴 显式覆盖用的网络端口。默认 null。
 * C13~C18 期间这是【唯一】的注入口，而生产代码里没人调用它 ⇒ 回溯腿永远
 * 停在 'no-http-port'。C19 之后它退化成一个测试接缝：真正的生产端口由
 * resolveHttpPort() 现场从「一个活着的、已登录的平台标签页」上造出来。
 */
let backfillTransport: HttpPort | null = null;
let lastTick: TickResult | null = null;
let pendingTick: Promise<unknown> = Promise.resolve();
/** 测试接缝：注入假时钟/自定义节奏，免得测试真的睡满 20 秒。生产恒为 null。 */
let backfillPaceOverride: { pace?: BackfillOptions['pace']; clock?: BackfillOptions['clock'] } | null = null;

export function configureBackfillTransport(http: HttpPort | null): void {
  backfillTransport = http;
}

export function configureBackfillPace(
  override: { pace?: BackfillOptions['pace']; clock?: BackfillOptions['clock'] } | null,
): void {
  backfillPaceOverride = override;
}

/** 最近一次 tick 的结果（给测试/排查用，也给 C18 的 Popup 用）。 */
export function lastBackfillTick(): TickResult | null {
  return lastTick;
}

function alarmsApi(): AlarmsApi | null {
  return (browser as unknown as { alarms?: AlarmsApi }).alarms ?? null;
}

function tabsApi(): { sendMessage: TabSend } | null {
  const tabs = (browser as unknown as { tabs?: { sendMessage?: TabSend } }).tabs;
  // 🔴 tabs.sendMessage 不需要 'tabs' 权限（'tabs' 只管 url/title 这些敏感字段），
  //    而且我们只往【自己注入的内容脚本】发消息。matches 一个字都不用改。
  return tabs && typeof tabs.sendMessage === 'function'
    ? { sendMessage: (id, msg) => tabs.sendMessage!(id, msg) }
    : null;
}

/**
 * 🔴 **生产环境里 http 端口就是在这里被造出来的。**
 *
 * 顺序：显式覆盖（测试） → 指定的那个标签页 → 登记表里任意一个还活着的同源标签页。
 * 一个都没有 ⇒ 返回 undefined ⇒ tickBackfill 如实答 'no-http-port'。
 * 🔴 绝不退化成"由 SW 自己 fetch" —— 那需要 host 权限，也会把取数挪出用户的
 *    登录上下文。宁可这一次不跑。
 */
async function resolveHttpPort(origin: string, senderTabId?: number): Promise<HttpPort | undefined> {
  if (backfillTransport) return backfillTransport;
  const tabs = tabsApi();
  if (!tabs) return undefined;
  // 实时腿那条路上，发消息的那个标签页【此刻就开着、且刚刚发生过一次真实抓取】
  // ——它是最可靠的取数通道，不必再去查登记表。
  if (senderTabId !== undefined) return tabHttpPort(senderTabId, tabs.sendMessage);
  const live = await pickLiveTab(browserLocalStore(), origin, (id) =>
    tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
  return live ? tabHttpPort(live.tabId, tabs.sendMessage) : undefined;
}

/**
 * C18 · Popup 问 background 要的运行时事实。全部是「我这边真实是什么」，不含推测。
 * 🔴 transportWired 不是一个静态标志，而是【现场 ping 一次】的结果：
 *    有没有一个活着的平台标签页可以替我们取数。没有就是没有，Popup 照实说。
 */
export async function backfillRuntimeStatus(): Promise<BackfillRuntimeStatus> {
  // 🔴 C33：一次 ping 同时回答两个问题（通道通不通 / 是哪个平台）。
  //    分两次问会出现「说有通道、却答不上是哪个平台」这种自相矛盾的回答。
  const live = await liveTransport();
  return {
    transportWired: live.wired,
    lastTickReason: lastTick?.reason ?? null,
    liveTarget: live.target,
  };
}

/**
 * 🔴 W2 · Popup 问的那一句「主机在不在」。
 *
 * 由 background 去问（而不是 Popup 自己探测）有两个理由：写完就落在
 * storage.local 里，Popup 关掉再打开仍然看得见上一次的结论；而且只有这一处
 * 会去碰主机，Popup 那边一行网络/socket 代码都没有。
 */
export async function popupHostStatus(): Promise<BackfillRuntimeStatus> {
  const base = await backfillRuntimeStatus();
  return { ...base, nativeHost: await hostStatusForPopup() };
}

/**
 * 此刻那条活着的取数通道。
 * `wired` 与 C19 的 hasLiveTransport 逐字同义；`target` 是它顺带带出来的事实：
 * 那个标签页的源，以及源在平台表里对应的平台。
 * 🔴 显式覆盖（测试接缝）下 target 恒为 null —— 那条路上没有任何标签页，
 *    编一个平台出来就是撒谎。
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
  // 源不在平台表里 ⇒ 我们答不上"这是哪个平台" ⇒ 照实说 null，绝不猜一个。
  return { wired: true, target: row ? { platform: row.id, origin: live.origin } : null };
}

/**
 * 🔴 C33 · 【显式知情同意】那条登记入口：用户在 Popup 上按了「开始回溯这个平台」。
 *
 * 与 kickBackfill 的关系：**这是多一条登记入口，不是放宽任何既有判定。**
 * kickBackfill / rememberTab / 闹钟里 `targets.length` 的判定一个字都没动 ——
 * 这里做的事和 kickBackfill 里那一行 rememberTarget 完全相同，只是目标的来源
 * 从「一次真实捕获」换成了「用户自己按下的这一次」。两个来源都不是我们编的。
 *
 * 🔴 scope（账号标识）怎么办：**记 'default'**，理由写在这里，不许悄悄编一个。
 *  · 通道那一跳（内容脚本 ping）带回来的只有源，**没有任何账号信息** ——
 *    要拿到真实账号得去读页面里的响应体，那是"猜"，本单不做；
 *  · 'default' 不是新发明：它就是本仓「认不出账号」时既有的写法
 *    （entrypoints/background.ts:303 的 `identity.value || 'default'`、
 *      lib/popup-view.ts 的 emptyStateFor、lib/contract.ts 的 IdentityLevel 'default'），
 *    含义逐字是「身份不可靠」，而不是冒充成某个具体账号；
 *  · 取数用的仍然是用户自己那个页面的登录态，所以**补回来的确实是他自己的历史**；
 *    scope 只是欠账账本的分区键，写 'default' 不会把别人的东西补到这里来。
 *  · 代价（如实写下）：以后真的捕获到一次时会再记一份带真实账号的目标，
 *    于是同一个平台下会有两份欠账集合，同一批会话可能被各自清一遍。
 *    文件名是 platform-sessionId、下载是覆盖写 ⇒ 不会串档、不会互相抹掉，
 *    多出来的只是重复的取数与写入。相比「回溯永远不开始」，这个代价是划算的。
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
  // 🔴 scope = 'default'：见上面那段。这是既有约定，不是新发明的值。
  const target = { platform: live.target.platform, origin: live.target.origin, scope: 'default' };
  await rememberTarget(store, { ...target, at: Date.now() });
  return { ok: true, target };
}

/** 等待 fire-and-forget 的那次 tick 结束。回溯腿绝不允许拖慢落盘，所以只能这样等。 */
export function backfillTickSettled(): Promise<unknown> {
  return pendingTick;
}

/** 从一次真实捕获里推出回溯目标。推不出来就返回 null（不猜）。 */
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
    } catch { /* 不是合法 URL ⇒ 换下一个候选 */ }
  }
  if (!origin) return null;
  // 归档范围键走 ADR-002 的账号轴；认不出账号就用 'default'（与落盘那边一致）。
  const identity = extractIdentity(captured.text, extractSessionId(captured.url, captured.text, captured.pageUrl));
  return { platform: row.id, origin, scope: identity.value || 'default' };
}

/**
 * 实时腿存完一条之后顺带踢一脚回溯腿。全程 best-effort：
 * 任何异常只进日志，绝不影响用户当前这条对话的落盘。
 */
export async function kickBackfill(
  captured: CapturedFetch,
  senderTabId?: number,
): Promise<TickResult | null> {
  const target = backfillTargetFor(captured);
  if (!target) return null;
  const store = browserLocalStore();
  // 闹钟醒来时用得上：把这个「用户真的用过的」目标记下来，省得以后去猜。
  // 记不下来也照跑这一次 —— 登记表只影响闹钟那条路。
  try {
    await rememberTarget(store, { ...target, at: Date.now() });
  } catch (err) {
    console.warn('[chat-stasher] backfill target registry write failed', (err as Error).message);
  }
  const result = await tickBackfill({
    ...target,
    store,
    http: await resolveHttpPort(target.origin, senderTabId),
    // 归档出口 = 回溯腿自己的投递函数（**不进发件箱**，见 §10 与 deliverBackfillItem）。
    // 🔴 C20：**必须 return**。以前这里是 `{ await handleCaptured(c); }` —— 花括号
    //    把 HandledResult 吃掉了，于是 engine 拿不到任何异议，没落盘也照样清账。
    sink: (c) => deliverBackfillItem(c),
    ...(backfillPaceOverride ?? {}),
  });
  lastTick = result;
  return result;
}

/**
 * 🔴 闹钟那一脚。与 kickBackfill 走【同一个 tickBackfill】，闸门顺序一致。
 * 与实时腿的唯一区别：目标从 storage 的登记表里读，而不是从一条消息里现取。
 * 一次闹钟最多清 1 笔账（DEFAULT_TICK_DETAILS），跑成了就收手。
 */
export async function runAlarmTick(): Promise<TickResult> {
  const store = browserLocalStore();

  // 🔴 W2 · 每一次闹钟醒来都先把发件箱送一遍（任务 5 的第一个时机）。
  //    顺序放在回溯腿之前：spool 里躺着的是用户当时就看着的那条对话，
  //    而回溯腿补的是历史 —— 手上这条先送出去。两条腿互不影响。
  const drain = await drainSafely();
  markOutboxDrain(drain);
  await syncOutboxAlarmSafely();
  await refreshBadgeSafely();

  const targets = await loadTargets(store);

  if (targets.length === 0) {
    // 还没有任何目标 ⇒ 用户从没在受支持平台上被抓到过一条。
    // 仍然把闸门跑一遍，好让 Popup 的 lastTickReason 说得出真话。
    const blocked = await tickBlockReason({
      hasStore: store !== null,
      isEnabled: () => isBackfillEnabled(store),
      isHostPaused: () => isBackfillHostPaused(store),
      // 🔴 C30：这两个值都是【事实】。以前这里把 hasHttp 写死成 false 再兜底成
      //    'no-http-port'，于是「通道好端端接着、只是没有目标」被报成了端口坏了 ——
      //    排查的人顺着那句话去查端口，而端口根本没坏。
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
      // 🔴 C20：闹钟那一脚同样必须 return（两条心跳走同一个出口，不许一条报一条不报）。
      sink: (c) => deliverBackfillItem(c),
      ...(backfillPaceOverride ?? {}),
    });
    last = result;
    lastTick = result;
    // 跑动了就收手；被开关/存储/主机暂停挡住也没必要再试别的目标（结论一样）。
    if (result.reason !== 'no-http-port') break;
  }
  await recordAlarmTick(store, last, targets.length);
  return last;
}

/** 闸门用的「现在是不是暂停态」——只读，不试恢复（恢复是 tickBackfill 的事）。 */
async function isBackfillHostPaused(store: ReturnType<typeof browserLocalStore>): Promise<boolean> {
  return (await loadHostPause(store)) !== null;
}

/**
 * 最近一次发件箱排空的结论（内存态，只给测试与排查用）。
 * 🔴 与回溯腿的 lastTick 一样：SW 被回收就没了 —— 发件箱里有什么从来只由
 *    发件箱自己说了算，这里不承载任何真相。
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
 * 🔴 C30 · 把闹钟这一跳的结论写进存储。
 *
 * 为什么必须落盘而不是只留在 lastTick 变量里：MV3 的 SW 干完这一跳就会被回收，
 * 用户过一会儿点开 Popup 时内存里什么都不剩 —— 真机上看到的正是这个：
 * 闹钟一直在醒、一直什么都没做，而任何地方都查不到它醒过。
 * 写失败只 warn，绝不让「留痕」反过来变成挡住这条腿的新理由。
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

/** 开关是什么状态，闹钟就该是什么状态。SW 每次醒来都对一次表。 */
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

export default defineBackground(async () => {
  browser.runtime.onMessage.addListener(
    (message: { type?: string; payload?: CapturedFetch }, sender, sendResponse) => {
      // C18：Popup 打开时问一句「取数通道接上没有」。
      // C19 起这个回答要现场 ping 一个标签页 ⇒ 变成异步，仍然 return true。
      // 🔴 W2：同一个回答里带上「主机在不在」——由 background 去问主机并把结论
      //    落盘，Popup 只负责把拿到的事实显示出来（它自己一行探测代码都没有）。
      if (message?.type === POPUP_STATUS_MESSAGE) {
        popupHostStatus()
          .then(sendResponse)
          // 问不出来就按最保守的方向答，绝不让 Popup 显示成"在跑"。
          .catch(() => sendResponse({ transportWired: false, lastTickReason: null, nativeHost: null }));
        return true;
      }
      // C19：内容脚本报到。tab id 由浏览器填在 sender 上（不需要 'tabs' 权限），
      // 记进 storage 之后，闹钟醒来才知道该找谁取数。
      // 🔴 C33：Popup 上那个「开始回溯这个平台」按钮按下来的那一脚。
      //    登记【写完】才回话 —— 回一个 ok 就该意味着"登记表里真的有这一条了"，
      //    否则 Popup 紧接着的那次重画会读到一张还没落盘的表，看起来像没生效。
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
        // 🔴 登记【写完】才回话：回一个 ok 就该意味着"闹钟已经找得到你了"，
        // 否则紧跟着的一次闹钟/状态查询会读到一张还没落盘的表。
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
          // C13：实时腿这一脚顺带唤醒回溯腿。fire-and-forget —— 回溯是慢活，
          // 绝不允许它挡住 sendResponse 或拖慢用户当前这条对话的落盘。
          pendingTick = kickBackfill(payload, senderTabId).catch((err) => {
            console.warn('[chat-stasher] backfill tick failed', (err as Error).message);
            return null;
          });
          // Privacy rule: never the conversation content — ids/bytes only.
          if (result.saved) {
            console.log(`[chat-stasher] acknowledged ${result.bytes} bytes as ${result.finalName}`);
          } else {
            console.log(
              `[chat-stasher] not delivered yet (${result.status}: ${result.reason ?? 'no reason given'})`
              + ` — ${result.bytes ?? 0} bytes`,
            );
          }
          // 🔴 `ok` 仍然是 `saved` 的别名，但 payload 里多了可区分的 `status`：
          //    调用方（内容脚本）再也看不到「没 ack 也算成功」这种回答了。
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

  // 🔴 C19 · 闹钟的监听器必须在 SW 顶层【同步】注册：MV3 里 SW 被回收后是由
  //    事件重新唤醒的，注册晚了就会错过那次唤醒。
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

  // Every SW wake (fresh start AND runtime.onStartup) re-asserts the badge's
  // truth, so a dead-worker leftover badge gets cleared once 5 min pass.
  void refreshBadge();
  // 开关是持久的，闹钟也该是。每次 SW 醒来对一次表：开着就确保有闹钟，
  // 关着就确保没有 —— 🔴 默认（关）下这里只会 clear，绝不会凭空创建。
  await syncAlarmWithSwitch().catch((err) => {
    console.warn('[chat-stasher] backfill alarm sync failed', (err as Error).message);
  });
  // The outbox timer follows the outbox, not the switch (§10).
  await syncOutboxAlarmSafely();
  browser.runtime.onStartup.addListener(() => {
    void refreshBadge();
    void syncAlarmWithSwitch().catch(() => { /* 日志已在上面那条路径覆盖 */ });
    void syncOutboxAlarmSafely();
  });

  console.log(
    '[chat-stasher] background ready: captures are queued in the outbox and delivered'
    + ' to the chat-stasher native host; nothing is ever downloaded',
  );
});
